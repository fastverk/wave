//! `wave discover` — find out-of-date dependencies across a group.
//!
//! Reuses the same forge/enumeration/manifest pipeline as `propose`: enumerate
//! the group, read each repo's manifest into [`RepoNode`](wave_core::RepoNode)s,
//! then hand them to [`find_candidates`](wave_core::find_candidates) with a set
//! of registry datasources. Read-only — it never writes to a forge (opening MRs
//! is Phase 2).
//!
//! By default this reports only *external* (3rd-party) deps: internal `@aion/*` /
//! `@savvi-studio/*` packages are left to the cascade — pass them via
//! `--internal-prefix` so cross-group scopes are skipped.
//!
//! `--include-internal` opts those prefixes back in, which is how a repo's own
//! first-party pins get brought up to latest. That needs a registry that can
//! answer for them: `--npm-scope-registry` routes a scope at a private registry
//! (GitLab's group npm endpoint is packument-compatible), mirroring `.npmrc`.

use anyhow::{Context, Result};
use wave_core::{find_candidates, Datasource, DiscoverConfig, ProviderChain};

use crate::datasource::{CargoDatasource, NpmDatasource};
use crate::{enumerate, forge_factory, open, render, DiscoverArgs};

/// Parse a `--npm-scope-registry '@scope/=https://host/path'` pair.
fn parse_scope_registry(spec: &str) -> Result<(String, String)> {
    let (scope, registry) = spec.split_once('=').with_context(|| {
        format!("--npm-scope-registry expects '@scope/=<registry-url>', got {spec:?}")
    })?;
    if scope.is_empty() || registry.is_empty() {
        anyhow::bail!("--npm-scope-registry expects '@scope/=<registry-url>', got {spec:?}");
    }
    Ok((scope.to_string(), registry.to_string()))
}

pub async fn run(a: &DiscoverArgs) -> Result<()> {
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
        // --repos still needs --group: it's the owner/namespace each repo lives
        // under (e.g. `fastverk/<repo>`), used to address the forge API.
        if a.group.is_empty() {
            anyhow::bail!("--repos also needs --group <org/group> (the owner the repos live under)");
        }
        a.repos
            .iter()
            .map(|n| enumerate::RepoSpec { name: n.clone() })
            .collect()
    };

    let chain = ProviderChain::default_chain();
    let nodes = enumerate::assemble_nodes(forge.as_ref(), &specs, &host, &a.group, &chain).await?;

    let http = reqwest::Client::builder()
        // crates.io requires a descriptive User-Agent.
        .user_agent("wave (+https://github.com/fastverk/wave)")
        .build()
        .context("build http client")?;
    // Scope overrides route first-party packages at their private registry. The
    // same forge token authorizes them (GitLab's npm registry lives on the forge
    // host), so there is no second credential to wire.
    let mut npm = NpmDatasource::new(http.clone());
    for spec in &a.npm_scope_registries {
        let (scope, registry) = parse_scope_registry(spec)?;
        npm = npm.with_scope(scope, registry, Some(token.clone()));
    }
    let datasources: Vec<Box<dyn Datasource>> =
        vec![Box::new(npm), Box::new(CargoDatasource::new(http))];

    if (a.include_internal || a.only_internal) && a.npm_scope_registries.is_empty() {
        // Fail loudly rather than emit a silently-empty report: the public
        // registry 404s every first-party package, and a lookup miss is not an
        // error (it's "no info"), so the run would look clean while doing nothing.
        tracing::warn!(
            "--include-internal without --npm-scope-registry: first-party packages \
             will be looked up on the public registry and almost certainly 404"
        );
    }

    let cfg = DiscoverConfig {
        internal_prefixes: a.internal_prefixes.clone(),
        force: a.force,
        include_internal: a.include_internal,
        only_internal: a.only_internal,
    };
    let candidates = find_candidates(&nodes, &datasources, &cfg).await?;

    if a.json {
        println!("{}", render::render_discovery_json(&candidates)?);
    } else {
        print!("{}", render::render_discovery(&candidates));
    }

    if a.open {
        let opened =
            open::open_candidates(forge.as_ref(), &chain, &candidates, &a.open_branch, a.auto_merge)
                .await?;
        for o in &opened {
            let verb = if o.already_existed { "refreshed" } else { "opened" };
            println!(
                "{verb} {} ({} bump(s)): {}",
                wave_core::repo_key(&o.repo),
                o.bumped.len(),
                o.url
            );
        }
        if opened.is_empty() {
            println!("nothing to open — every candidate is already on the branch");
        }
    }
    Ok(())
}
