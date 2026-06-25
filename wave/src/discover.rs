//! `wave discover` — find out-of-date *external* dependencies across a group.
//!
//! Reuses the same forge/enumeration/manifest pipeline as `propose`: enumerate
//! the group, read each repo's manifest into [`RepoNode`](wave_core::RepoNode)s,
//! then hand them to [`find_candidates`](wave_core::find_candidates) with a set
//! of registry datasources. Read-only — it never writes to a forge (opening MRs
//! is Phase 2). Internal `@aion/*` / `@savvi-studio/*` packages are left to the
//! cascade; pass them via `--internal-prefix` so cross-group scopes are skipped.

use anyhow::{Context, Result};
use wave_core::{find_candidates, Datasource, DiscoverConfig, ProviderChain};

use crate::datasource::NpmDatasource;
use crate::{enumerate, forge_factory, render, DiscoverArgs};

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
        a.repos
            .iter()
            .map(|n| enumerate::RepoSpec { name: n.clone() })
            .collect()
    };

    let chain = ProviderChain::default_chain();
    let nodes = enumerate::assemble_nodes(forge.as_ref(), &specs, &host, &a.group, &chain).await?;

    let http = reqwest::Client::builder()
        .user_agent("wave")
        .build()
        .context("build http client")?;
    // Phase 1: npm only. Cargo / Docker / BCR datasources land in Phase 3.
    let datasources: Vec<Box<dyn Datasource>> = vec![Box::new(NpmDatasource::new(http))];

    let cfg = DiscoverConfig {
        internal_prefixes: a.internal_prefixes.clone(),
        force: a.force,
    };
    let candidates = find_candidates(&nodes, &datasources, &cfg).await?;
    print!("{}", render::render_discovery(&candidates));
    Ok(())
}
