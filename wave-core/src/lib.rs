//! `wave-core` — the cross-repo dependency-cascade engine.
//!
//! A *wave* propagates a dependency-version bump through a graph of separate
//! repositories: an upstream publishes a new version, downstream repos open
//! bump MRs/PRs, auto-merge on green, publish their own new version, and the
//! cascade continues — in dependency order.
//!
//! This crate is the engine:
//! - [`provider`] parses each repo's manifest (Bazel `MODULE.bazel`, npm
//!   `package.json`, …) into the name it publishes + its dependency edges,
//!   via an ordered [`ProviderChain`];
//! - [`graph`] assembles the cross-repo DAG, computes the affected set + tiers,
//!   and emits a topologically-ordered [`WavePlan`] ([`propose`]);
//! - [`edge`] is the version-constraint model + bump policy.
//! - [`discover`] is the complement to [`propose`]: it polls registry
//!   [`Datasource`](discover::Datasource)s for newer *external* dependency
//!   versions and reports the repos whose pins need a bump.
//!
//! Actuation (opening/merging changes via a forge) is layered on top through
//! the `fastverk-forge` [`forge::Forge`] trait:
//! - [`manifest`] reads live manifests through a forge;
//! - [`runner`] materializes a plan into a durable [`pb::Wave`] and drives each
//!   repo through the cascade state machine ([`runner::WaveRunner`]);
//! - [`store`] persists waves as the `wave.v1` proto on disk.

/// Generated `wave.v1` proto types (messages only). Wave owns its store schema:
/// its `RepoRef`/`ChangeRef` mirror forge's but are wave's own prost types, so
/// wave's `@crates` prost stays independent of forge's across the Bazel module
/// boundary. [`to_pb_repo`]/[`to_forge_repo`]/[`to_pb_change`] convert at the
/// forge edge.
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/wave.v1.rs"));
}

pub mod discover;
pub mod edge;
pub mod graph;
pub mod manifest;
pub mod provider;
pub mod runner;
pub mod store;
pub mod trace;

pub use discover::{find_candidates, is_internal, Candidate, Datasource, DiscoverConfig, VersionInfo};
pub use edge::{
    decide, BumpDecision, DepEdge, EdgeKind, ManifestSource, VersionConstraint,
};
pub use graph::{propose, repo_key, PlanItem, RepoNode, WavePlan};
pub use manifest::ForgeManifestSource;
pub use provider::{
    BazelDepProvider, CargoProvider, GraphProvider, NpmProvider, PnpmCatalogProvider,
    ProviderChain,
};
pub use runner::{
    materialize, next_action, wave_id, ItemAction, NullObserver, WaveObserver, WaveRunner,
};
pub use store::Store;
pub use trace::{project, EventLog, LoggingObserver};

// ── forge ↔ wave RepoRef/ChangeRef conversions ──────────────────────────
// The forge `Forge` trait speaks `forge::RepoRef`/`forge::ChangeRef` (forge's
// prost types); wave stores its own `pb::RepoRef`/`pb::ChangeRef`. These convert
// at the boundary (a flat field copy — the shapes mirror each other).

/// A forge runtime `RepoRef` → wave's stored `RepoRef`.
#[must_use]
pub fn to_pb_repo(r: &forge::RepoRef) -> pb::RepoRef {
    pb::RepoRef {
        forge: r.forge,
        host: r.host.clone(),
        owner: r.owner.clone(),
        name: r.name.clone(),
    }
}

/// Wave's stored `RepoRef` → a forge runtime `RepoRef`.
#[must_use]
pub fn to_forge_repo(r: &pb::RepoRef) -> forge::RepoRef {
    forge::RepoRef {
        forge: r.forge,
        host: r.host.clone(),
        owner: r.owner.clone(),
        name: r.name.clone(),
    }
}

/// A forge `ChangeRef` → wave's stored `ChangeRef`.
#[must_use]
pub fn to_pb_change(c: &forge::ChangeRef) -> pb::ChangeRef {
    pb::ChangeRef {
        number: c.number,
        url: c.url.clone(),
        branch: c.branch.clone(),
    }
}

/// Wave's stored `ChangeRef` → a forge runtime `ChangeRef`.
#[must_use]
pub fn to_forge_change(c: &pb::ChangeRef) -> forge::ChangeRef {
    forge::ChangeRef {
        number: c.number,
        url: c.url.clone(),
        branch: c.branch.clone(),
    }
}

/// Stable key for a stored repo — same format as [`repo_key`] (so a forge repo
/// and its converted `pb` form key identically).
#[must_use]
pub fn pb_repo_key(r: &pb::RepoRef) -> String {
    format!("{}|{}/{}", r.host, r.owner, r.name)
}

/// `owner/name` for a stored repo (owner may be a nested group path).
#[must_use]
pub fn pb_repo_slug(r: &pb::RepoRef) -> String {
    if r.owner.is_empty() {
        r.name.clone()
    } else {
        format!("{}/{}", r.owner, r.name)
    }
}
