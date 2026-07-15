//! `wave` — the cross-repo dependency-cascade CLI/daemon.
//!
//! An upstream publishes a new version; `wave` opens version-bump changes in
//! every downstream repo, auto-merges them on green, and cascades the bump
//! through the dependency DAG in tier order.
//!
//! Tokens come from the environment (`GITLAB_TOKEN` / `GITHUB_TOKEN`, else
//! `FORGE_TOKEN`). Repos are enumerated from a `--group`/`--org` via the forge
//! REST API, or passed explicitly with `--repos a,b,c`.
//!
//! Subcommands: `propose`, `start`, `status`, `reconcile`, `trace`, `discover`.

mod datasource;
mod discover;
mod enumerate;
mod forge_factory;
mod open;
mod render;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use forge::ForgeKind;
use wave_core::{
    materialize, pb, project, propose, EventLog, LoggingObserver, ProviderChain, Store, WavePlan,
    WaveRunner,
};

#[derive(Parser)]
#[command(name = "wave", about = "Cross-repo dependency-cascade engine", version)]
struct Cli {
    /// Override the wave store path (default ~/.fastverk/wave-store.pb).
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Dry-run: compute the tiered cascade plan for bumping a module.
    Propose(PlanArgs),
    /// Open the cascade: materialize + persist + run one reconcile pass.
    Start(PlanArgs),
    /// Show waves (all, or one by id).
    Status {
        /// Wave id; omit to list all.
        id: Option<String>,
    },
    /// Drive in-flight waves one reconcile pass (idempotent + resumable).
    Reconcile {
        /// Wave id; omit to drive every active wave.
        id: Option<String>,
    },
    /// Render a wave's propagation trace as a tier-indented waterfall.
    Trace {
        /// Wave id.
        id: String,
    },
    /// Scan a group for out-of-date external (3rd-party) dependencies.
    Discover(DiscoverArgs),
}

#[derive(Args)]
struct PlanArgs {
    /// The module/package that published a new version.
    module: String,
    /// The new version to cascade.
    target: String,
    /// Forge: github | gitlab.
    #[arg(long, default_value = "gitlab")]
    forge: String,
    /// Instance host (e.g. gitlab.savvifi.com); empty = the forge default.
    #[arg(long, default_value = "")]
    host: String,
    /// Org (GitHub) or group path (GitLab) to enumerate. Ignored if --repos set.
    #[arg(long, default_value = "")]
    group: String,
    /// Explicit repo names (comma-separated) instead of enumerating a group.
    #[arg(long, value_delimiter = ',')]
    repos: Vec<String>,
    /// Force a bump even where a caret/range already admits the target.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
pub struct DiscoverArgs {
    /// Forge: github | gitlab.
    #[arg(long, default_value = "gitlab")]
    forge: String,
    /// Instance host (e.g. gitlab.savvifi.com); empty = the forge default.
    #[arg(long, default_value = "")]
    host: String,
    /// Org (GitHub) or group path (GitLab) to scan. Ignored if --repos set.
    #[arg(long, default_value = "")]
    group: String,
    /// Explicit repo names (comma-separated) instead of enumerating a group.
    #[arg(long, value_delimiter = ',')]
    repos: Vec<String>,
    /// Module-name prefixes treated as internal (owned by the cascade, skipped
    /// by discovery). Repeatable, e.g. `--internal-prefix @aion/
    /// --internal-prefix @savvi-studio/`.
    #[arg(long = "internal-prefix")]
    internal_prefixes: Vec<String>,
    /// Report bumps even where a caret/range already admits the latest version.
    #[arg(long)]
    force: bool,
    /// Also report the --internal-prefix modules, i.e. bring this repo's
    /// FIRST-PARTY pins up to the latest published version. Off by default, which
    /// keeps discovery and the cascade disjoint. A module published by one of the
    /// scanned repos stays the cascade's regardless.
    ///
    /// Pair with --force when the pins are carets: `^0.2.0` already admits
    /// `0.2.3`, so only --force advances the floor (and hence the lockfile).
    #[arg(long)]
    include_internal: bool,
    /// Route a scope at a private npm registry, in .npmrc's own form:
    /// `--npm-scope-registry '@aion/=https://gitlab.example.com/api/v4/groups/195/-/packages/npm'`.
    /// Repeatable. GitLab's group npm endpoint is packument-compatible, so it
    /// needs no separate datasource. Authorized by $GITLAB_TOKEN / $FORGE_TOKEN.
    #[arg(long = "npm-scope-registry")]
    npm_scope_registries: Vec<String>,
    /// WRITE: open (or refresh) one change per repo carrying its bumps. Without
    /// this, discover only reports. The branch is stable, so a re-run refreshes
    /// the same change rather than opening another.
    #[arg(long)]
    open: bool,
    /// The branch `--open` writes to. Stable on purpose — see --open.
    #[arg(long, default_value = "wave/dep-bumps")]
    open_branch: String,
    /// With --open, arm merge-when-pipeline-succeeds. Off by default: the change
    /// is opened and held for review.
    #[arg(long)]
    auto_merge: bool,
    /// Emit the candidates as JSON instead of the text report (read-only; a
    /// machine-readable plan to inspect before `--open`).
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let store_path = cli.store.unwrap_or_else(Store::default_path);
    match cli.cmd {
        Cmd::Propose(a) => {
            let plan = build_plan(&a).await?;
            print!("{}", render::render_plan(&plan));
            Ok(())
        }
        Cmd::Start(a) => start(&a, store_path).await,
        Cmd::Status { id } => status(&store_path, id.as_deref()),
        Cmd::Reconcile { id } => reconcile(&store_path, id.as_deref()).await,
        Cmd::Trace { id } => trace(&store_path, &id),
        Cmd::Discover(a) => discover::run(&a).await,
    }
}

/// Enumerate the group (or use --repos), read manifests, compute the plan.
async fn build_plan(a: &PlanArgs) -> Result<WavePlan> {
    let kind = forge_factory::parse_forge_kind(&a.forge)?;
    let host = forge_factory::effective_host(kind, &a.host);
    let token = forge_factory::token_for(kind)?;
    let forge = forge_factory::build_forge(kind, &host, &token)?;

    let specs = if a.repos.is_empty() {
        if a.group.is_empty() {
            anyhow::bail!("pass --group <org/group> or --repos a,b,c");
        }
        enumerate::enumerate(kind, &host, &a.group, &token).await?
    } else {
        a.repos
            .iter()
            .map(|n| enumerate::RepoSpec { name: n.clone() })
            .collect()
    };

    let chain = ProviderChain::default_chain();
    let nodes = enumerate::assemble_nodes(forge.as_ref(), &specs, &host, &a.group, &chain).await?;
    Ok(propose(nodes, &a.module, &a.target, a.force))
}

async fn start(a: &PlanArgs, store_path: PathBuf) -> Result<()> {
    let plan = build_plan(a).await?;
    let mut wave = materialize(&plan, a.force);
    let store = Store::open(store_path)?;
    store.upsert(wave.clone())?;
    println!("opened {} ({} items)", wave.id, wave.items.len());

    let kind = forge_factory::parse_forge_kind(&a.forge)?;
    let host = forge_factory::effective_host(kind, &a.host);
    let token = forge_factory::token_for(kind)?;
    let forge = forge_factory::build_forge(kind, &host, &token)?;
    let chain = ProviderChain::default_chain();
    let observer = LoggingObserver::new(EventLog::for_wave(&EventLog::default_dir(), &wave.id));
    let runner = WaveRunner::new(forge.as_ref(), &chain, &observer);
    runner.reconcile(&mut wave).await?;
    store.upsert(wave.clone())?;
    print!("{}", render::render_wave(&wave));
    Ok(())
}

fn status(store_path: &Path, id: Option<&str>) -> Result<()> {
    let store = Store::open(store_path.to_path_buf())?;
    match id {
        Some(id) => {
            let wave = store.get(id).with_context(|| format!("no wave {id}"))?;
            print!("{}", render::render_wave(&wave));
        }
        None => print!("{}", render::render_wave_list(&store.waves())),
    }
    Ok(())
}

async fn reconcile(store_path: &Path, id: Option<&str>) -> Result<()> {
    let store = Store::open(store_path.to_path_buf())?;
    let waves: Vec<pb::Wave> = match id {
        Some(id) => vec![store.get(id).with_context(|| format!("no wave {id}"))?],
        None => store.waves().into_iter().filter(is_active).collect(),
    };
    if waves.is_empty() {
        println!("no active waves");
        return Ok(());
    }

    let chain = ProviderChain::default_chain();
    for mut wave in waves {
        let Some(anchor) = wave
            .items
            .first()
            .and_then(|i| i.repo.clone())
            .or_else(|| wave.root_repo.clone())
        else {
            continue;
        };
        let kind = ForgeKind::try_from(anchor.forge).unwrap_or(ForgeKind::Unspecified);
        let host = forge_factory::effective_host(kind, &anchor.host);
        let token = forge_factory::token_for(kind)?;
        let forge = forge_factory::build_forge(kind, &host, &token)?;
        let observer = LoggingObserver::new(EventLog::for_wave(&EventLog::default_dir(), &wave.id));
        let runner = WaveRunner::new(forge.as_ref(), &chain, &observer);
        runner.reconcile(&mut wave).await?;
        store.upsert(wave.clone())?;
        print!("{}", render::render_wave(&wave));
    }
    Ok(())
}

fn trace(store_path: &Path, id: &str) -> Result<()> {
    let store = Store::open(store_path.to_path_buf())?;
    let wave = store.get(id).with_context(|| format!("no wave {id}"))?;
    let events = EventLog::for_wave(&EventLog::default_dir(), id).read()?;
    let trace = project(&wave, &events);
    print!("{}", render::render_trace(&trace));
    Ok(())
}

fn is_active(w: &pb::Wave) -> bool {
    use pb::WaveState as S;
    !matches!(
        S::try_from(w.state).unwrap_or(S::Unspecified),
        S::Completed | S::Aborted
    )
}
