//! External-dependency *discovery* — the complement to [`propose`](crate::propose).
//!
//! `propose` *propagates* a bump you already know about (an internal upstream
//! published a new version) through the cross-repo DAG. Discovery does the other
//! half: it scans the dependency edges already parsed for each repo, asks a
//! registry [`Datasource`] what the latest published version of each *external*
//! (3rd-party) dependency is, and emits the repos whose pinned constraint would
//! need a bump to reach it.
//!
//! The internal/external split is the crux: a dependency is **internal** (owned
//! by the cascade, left alone here) if some enumerated repo publishes it, or its
//! name matches a configured internal prefix (`@aion/`, `@savvi-studio/`, …).
//! Everything else is **external** and a discovery candidate. Keeping the two
//! disjoint is what lets discovery and the cascade coexist without fighting over
//! the same manifests.
//!
//! Like [`ManifestSource`](crate::ManifestSource) and
//! [`GraphProvider`](crate::GraphProvider), [`Datasource`] is a pure seam: the
//! engine logic here does no I/O of its own — network access happens only
//! through the trait, whose concrete (HTTP) implementations the caller wires in.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use async_trait::async_trait;
use forge::RepoRef;
use futures::stream::StreamExt;

use crate::edge::{decide, BumpDecision, EdgeKind, VersionConstraint};
use crate::graph::{repo_key, RepoNode};

/// The latest published version of an external dependency, from a registry.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// The latest version string (e.g. npm `dist-tags.latest`).
    pub version: String,
    /// For digest-pinned ecosystems (Docker / CI images): the current digest.
    pub digest: Option<String>,
    /// Release timestamp, when the datasource reports one (RFC 3339).
    pub released_at: Option<String>,
}

/// A source of latest-version information for one dependency ecosystem
/// (npm registry, crates.io, a container registry, …). A pure seam: the engine
/// calls it; concrete HTTP impls are supplied by the caller.
#[async_trait]
pub trait Datasource: Send + Sync {
    /// The edge kind this datasource resolves.
    fn kind(&self) -> EdgeKind;
    /// The latest version of `package`, or `None` if it is unknown / not found.
    async fn latest(&self, package: &str) -> Result<Option<VersionInfo>>;
}

/// What separates an internal dependency (owned by the cascade) from an external
/// one (a discovery candidate).
#[derive(Debug, Clone, Default)]
pub struct DiscoverConfig {
    /// Module-name prefixes that mark a dependency as internal and therefore
    /// skipped — e.g. `@aion/`, `@savvi-studio/`. A dependency published by one
    /// of the enumerated repos is treated as internal automatically, without a
    /// prefix; prefixes cover internal scopes whose producer is in *another*
    /// group than the one being scanned.
    pub internal_prefixes: Vec<String>,
    /// Force a bump even where a caret/range already admits the latest version
    /// (advance the manifest floor rather than rely on lockfile maintenance).
    pub force: bool,
    /// Opt-in: also poll+report the modules matched by [`Self::internal_prefixes`],
    /// i.e. bring a repo's *first-party* pins up to the latest published version.
    ///
    /// This deliberately relaxes only the **prefix** rule. A module published by
    /// one of the enumerated repos stays internal regardless — that one is owned
    /// by the cascade, and letting discovery bump it too is exactly the
    /// double-ownership this partition exists to prevent. A prefix-matched module
    /// with no producer in the scan has no such owner, so polling it is safe.
    ///
    /// Default `false` keeps discovery and the cascade disjoint, as before.
    pub include_internal: bool,
    /// Restrict the report to modules matching [`Self::internal_prefixes`] —
    /// "bring THIS repo's first-party pins up to latest" WITHOUT dragging every
    /// third-party dependency into the same change. Implies
    /// [`Self::include_internal`].
    ///
    /// Without this, opting internal in *adds* to the external set rather than
    /// selecting it, so a first-party bump would also carry unrelated registry
    /// churn — a different change, with a different blast radius and a different
    /// reviewer.
    pub only_internal: bool,
}

impl DiscoverConfig {
    /// Are prefix-matched modules poll-eligible? Either mode opts them in.
    fn internal_opted_in(&self) -> bool {
        self.include_internal || self.only_internal
    }

    /// Does `module` match a configured internal prefix?
    fn matches_internal_prefix(&self, module: &str) -> bool {
        self.internal_prefixes
            .iter()
            .any(|p| module.starts_with(p.as_str()))
    }

    /// Is `module` filtered out by [`Self::only_internal`]?
    fn excluded_by_only_internal(&self, module: &str) -> bool {
        self.only_internal && !self.matches_internal_prefix(module)
    }
}

/// One external-dependency update opportunity for one repo.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub repo: RepoRef,
    /// The external dependency's name.
    pub module: String,
    /// The manifest the edge lives in (the file a bump would rewrite).
    pub manifest_path: String,
    pub kind: EdgeKind,
    /// The repo's current constraint on the dependency.
    pub current: VersionConstraint,
    /// The latest version the datasource reported.
    pub latest: String,
    /// Why this is a candidate (always [`BumpDecision::NeedsBump`] today; kept
    /// explicit so a future report can surface conflicts too).
    pub decision: BumpDecision,
}

/// Is `module` internal — owned by the cascade, not a discovery candidate?
///
/// Published-by-a-scanned-repo always wins: that module has a producer in this
/// very scan, so the cascade owns it. [`DiscoverConfig::include_internal`] only
/// relaxes the weaker prefix rule (see that field).
#[must_use]
pub fn is_internal(module: &str, published: &HashSet<String>, cfg: &DiscoverConfig) -> bool {
    published.contains(module) || (!cfg.internal_opted_in() && cfg.matches_internal_prefix(module))
}

/// Max concurrent registry lookups. Registries (npm, crates.io sparse index)
/// tolerate this comfortably; it keeps an org-wide scan from serializing on
/// network latency.
const POLL_CONCURRENCY: usize = 16;

/// Scan assembled `nodes` for external-dependency updates, polling each matching
/// [`Datasource`]. Each distinct `(kind, module)` is polled once, and the
/// lookups run concurrently. Edges that are internal, non-semver (`workspace:*`,
/// URLs), prerelease-only, or have no datasource for their ecosystem are
/// skipped. A lookup that errors is treated as "no info" (logged, not fatal) so
/// one flaky registry response can't sink the whole scan. Returns one
/// [`Candidate`] per (repo, dependency) that needs a bump, stably ordered by
/// repo then module.
pub async fn find_candidates(
    nodes: &[RepoNode],
    datasources: &[Box<dyn Datasource>],
    cfg: &DiscoverConfig,
) -> Result<Vec<Candidate>> {
    // Every module published by an enumerated repo is internal by construction.
    let published: HashSet<String> = nodes.iter().filter_map(|n| n.published.clone()).collect();

    // The distinct (kind, module) pairs that warrant a registry lookup: external,
    // semver-bearing, and backed by a datasource. Deduped so each is polled once.
    let mut to_poll: HashSet<(EdgeKind, String)> = HashSet::new();
    for node in nodes {
        for edge in &node.edges {
            if matches!(edge.current, VersionConstraint::Other(_)) {
                continue;
            }
            if is_internal(&edge.module, &published, cfg) {
                continue;
            }
            if cfg.excluded_by_only_internal(&edge.module) {
                continue;
            }
            if datasources.iter().any(|d| d.kind() == edge.kind) {
                to_poll.insert((edge.kind, edge.module.clone()));
            }
        }
    }

    // Poll concurrently (bounded). `None` = looked up, no usable stable version.
    let cache: HashMap<(EdgeKind, String), Option<semver::Version>> =
        futures::stream::iter(to_poll)
            .map(|(kind, module)| async move {
                let latest = match datasources.iter().find(|d| d.kind() == kind) {
                    Some(ds) => resolve_latest(ds.as_ref(), &module).await.unwrap_or_else(|e| {
                        tracing::warn!("datasource lookup for {module} failed: {e:#}");
                        None
                    }),
                    None => None,
                };
                ((kind, module), latest)
            })
            .buffer_unordered(POLL_CONCURRENCY)
            .collect()
            .await;

    // Build candidates from the cached lookups (pure, no I/O).
    let mut candidates: Vec<Candidate> = Vec::new();
    for node in nodes {
        for edge in &node.edges {
            // Same guard as the poll loop above. A non-semver spec (`catalog:`,
            // `workspace:*`, a URL) carries no version here to compare against or
            // rewrite — the real version lives elsewhere (pnpm's catalog block) or
            // nowhere. This used to hold implicitly: nothing polled such a module,
            // so the cache miss skipped it. Once a SIBLING edge polls the same
            // module (a catalog entry and a `"dep": "catalog:"` pin name the same
            // package), the opaque edge would ride in on that cache hit and
            // propose rewriting `"catalog:"` into a literal version — destroying
            // the indirection. Skip explicitly.
            if matches!(edge.current, VersionConstraint::Other(_)) {
                continue;
            }
            // Mirror the poll loop's filter. Relying on the cache miss alone is
            // what let the opaque-spec bug through, so both guards are explicit
            // in both loops.
            if cfg.excluded_by_only_internal(&edge.module) {
                continue;
            }
            let Some(Some(latest)) = cache.get(&(edge.kind, edge.module.clone())) else {
                continue;
            };
            let decision = decide(&edge.current, latest, cfg.force);
            if decision == BumpDecision::NeedsBump {
                candidates.push(Candidate {
                    repo: node.repo.clone(),
                    module: edge.module.clone(),
                    manifest_path: edge.manifest_path.clone(),
                    kind: edge.kind,
                    current: edge.current.clone(),
                    latest: latest.to_string(),
                    decision,
                });
            }
        }
    }

    candidates.sort_by(|a, b| {
        repo_key(&a.repo)
            .cmp(&repo_key(&b.repo))
            .then_with(|| a.module.cmp(&b.module))
    });
    Ok(candidates)
}

/// Query a datasource and parse the latest version, dropping non-semver and
/// prerelease results (ignore-unstable by default — don't jump a stable pin onto
/// a prerelease).
async fn resolve_latest(ds: &dyn Datasource, module: &str) -> Result<Option<semver::Version>> {
    let Some(info) = ds.latest(module).await? else {
        return Ok(None);
    };
    let Ok(v) = semver::Version::parse(info.version.trim()) else {
        return Ok(None);
    };
    if v.pre.is_empty() {
        Ok(Some(v))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::{DepEdge, EdgeKind, VersionConstraint};
    use std::collections::HashMap;

    fn repo(name: &str) -> RepoRef {
        RepoRef {
            forge: forge::ForgeKind::Gitlab as i32,
            host: "gitlab.savvifi.com".into(),
            owner: "studio".into(),
            name: name.into(),
        }
    }

    fn npm_edge(module: &str, spec: &str) -> DepEdge {
        DepEdge {
            module: module.into(),
            current: VersionConstraint::parse_npm(spec),
            manifest_path: "package.json".into(),
            kind: EdgeKind::Npm,
        }
    }

    fn node(name: &str, publishes: Option<&str>, edges: Vec<DepEdge>) -> RepoNode {
        RepoNode {
            repo: repo(name),
            published: publishes.map(str::to_string),
            edges,
        }
    }

    /// A datasource that returns a fixed latest version per package.
    struct MockNpm(HashMap<String, String>);

    #[async_trait]
    impl Datasource for MockNpm {
        fn kind(&self) -> EdgeKind {
            EdgeKind::Npm
        }
        async fn latest(&self, package: &str) -> Result<Option<VersionInfo>> {
            Ok(self.0.get(package).map(|v| VersionInfo {
                version: v.clone(),
                digest: None,
                released_at: None,
            }))
        }
    }

    fn ds(pairs: &[(&str, &str)]) -> Vec<Box<dyn Datasource>> {
        let map = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        vec![Box::new(MockNpm(map))]
    }

    #[tokio::test]
    async fn external_dep_with_newer_latest_becomes_a_candidate() {
        let nodes = vec![node(
            "web",
            None,
            vec![npm_edge("zod", "^4.4.0")],
        )];
        let cands = find_candidates(&nodes, &ds(&[("zod", "5.1.0")]), &DiscoverConfig::default())
            .await
            .unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].module, "zod");
        assert_eq!(cands[0].latest, "5.1.0");
        assert_eq!(cands[0].decision, BumpDecision::NeedsBump);
    }

    #[tokio::test]
    async fn caret_already_admitting_latest_is_not_a_candidate() {
        // ^4.4.0 admits 4.4.9 → the manifest range is fine (lockfile's job).
        let nodes = vec![node("web", None, vec![npm_edge("zod", "^4.4.0")])];
        let cands = find_candidates(&nodes, &ds(&[("zod", "4.4.9")]), &DiscoverConfig::default())
            .await
            .unwrap();
        assert!(cands.is_empty());
    }

    #[tokio::test]
    async fn internal_dep_published_by_an_enumerated_repo_is_skipped() {
        // `@s/modules` is published by the `modules` repo → cascade-owned.
        let nodes = vec![
            node("modules", Some("@s/modules"), vec![]),
            node("web", None, vec![npm_edge("@s/modules", "^0.1.0")]),
        ];
        let cands =
            find_candidates(&nodes, &ds(&[("@s/modules", "0.9.0")]), &DiscoverConfig::default())
                .await
                .unwrap();
        assert!(cands.is_empty());
    }

    #[tokio::test]
    async fn internal_prefix_is_skipped_even_without_a_local_producer() {
        // @aion/* is produced in another group; the prefix marks it internal.
        let nodes = vec![node("web", None, vec![npm_edge("@aion/kernel", "^0.1.0")])];
        let cfg = DiscoverConfig {
            internal_prefixes: vec!["@aion/".into()],
            force: false,
            include_internal: false,
            only_internal: false,
        };
        let cands = find_candidates(&nodes, &ds(&[("@aion/kernel", "0.5.0")]), &cfg)
            .await
            .unwrap();
        assert!(cands.is_empty());
    }

    #[tokio::test]
    async fn include_internal_opts_a_prefixed_module_back_in() {
        // The same scan as above, with the opt-in: @aion/* is now a candidate —
        // this is "bring a repo's first-party pins up to latest".
        let nodes = vec![node("web", None, vec![npm_edge("@aion/kernel", "^0.1.0")])];
        let cfg = DiscoverConfig {
            internal_prefixes: vec!["@aion/".into()],
            force: false,
            include_internal: true,
            only_internal: false,
        };
        let cands = find_candidates(&nodes, &ds(&[("@aion/kernel", "0.5.0")]), &cfg)
            .await
            .unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].module, "@aion/kernel");
        assert_eq!(cands[0].latest, "0.5.0");
    }

    #[tokio::test]
    async fn include_internal_still_defers_to_a_producer_in_the_scan() {
        // include_internal relaxes only the PREFIX rule. A module published by a
        // scanned repo is the cascade's to bump — discovery must not also claim
        // it, or the two fight over the same manifest.
        let nodes = vec![
            node("kernel", Some("@aion/kernel"), vec![]),
            node("web", None, vec![npm_edge("@aion/kernel", "^0.1.0")]),
        ];
        let cfg = DiscoverConfig {
            internal_prefixes: vec!["@aion/".into()],
            force: false,
            include_internal: true,
            only_internal: false,
        };
        let cands = find_candidates(&nodes, &ds(&[("@aion/kernel", "0.5.0")]), &cfg)
            .await
            .unwrap();
        assert!(
            cands.is_empty(),
            "a producer in the scan keeps the module internal even with include_internal"
        );
    }

    #[tokio::test]
    async fn only_internal_selects_first_party_instead_of_adding_to_it() {
        // include_internal ADDS internal to the external set; only_internal
        // SELECTS it. The difference is a change carrying 10 first-party bumps vs
        // one carrying 10 + every third-party dep that drifted — a different blast
        // radius, and usually a different reviewer.
        let nodes = vec![node(
            "web",
            None,
            vec![
                npm_edge("@aion/kernel", "^0.1.0"),
                npm_edge("@aws-sdk/client-s3", "^3.0.0"),
            ],
        )];
        // Both targets must be OUTSIDE their caret, or the third-party one is
        // AlreadySatisfied and the test proves nothing about filtering.
        let ds = ds(&[("@aion/kernel", "0.5.0"), ("@aws-sdk/client-s3", "4.0.0")]);

        let both = DiscoverConfig {
            internal_prefixes: vec!["@aion/".into()],
            force: false,
            include_internal: true,
            only_internal: false,
        };
        let cands = find_candidates(&nodes, &ds, &both).await.unwrap();
        assert_eq!(cands.len(), 2, "include_internal reports first- AND third-party");

        let only = DiscoverConfig {
            internal_prefixes: vec!["@aion/".into()],
            force: false,
            include_internal: false, // implied by only_internal
            only_internal: true,
        };
        let cands = find_candidates(&nodes, &ds, &only).await.unwrap();
        assert_eq!(cands.len(), 1, "only_internal reports first-party alone");
        assert_eq!(cands[0].module, "@aion/kernel");
    }

    #[tokio::test]
    async fn an_opaque_spec_is_never_a_candidate_even_when_a_sibling_polls_it() {
        // The pnpm-catalog shape: package.json says `"dep": "catalog:"` (opaque)
        // while pnpm-workspace.yaml carries the real `^0.2.0`. Both edges name the
        // SAME module, so the catalog edge causes a poll — and the opaque edge must
        // not ride in on that cache hit. Bumping it would rewrite `"catalog:"` into
        // a literal version and destroy the indirection.
        let catalog_edge = DepEdge {
            module: "@aion/kernel".into(),
            current: VersionConstraint::parse_npm("^0.2.0"),
            manifest_path: "pnpm-workspace.yaml".into(),
            kind: EdgeKind::Npm,
        };
        let opaque_pin = DepEdge {
            module: "@aion/kernel".into(),
            current: VersionConstraint::parse_npm("catalog:"),
            manifest_path: "package.json".into(),
            kind: EdgeKind::Npm,
        };
        assert!(matches!(opaque_pin.current, VersionConstraint::Other(_)));

        let nodes = vec![node("web", None, vec![catalog_edge, opaque_pin])];
        let cfg = DiscoverConfig {
            internal_prefixes: vec!["@aion/".into()],
            force: true,
            include_internal: true,
            only_internal: false,
        };
        let cands = find_candidates(&nodes, &ds(&[("@aion/kernel", "0.2.3")]), &cfg)
            .await
            .unwrap();
        assert_eq!(cands.len(), 1, "only the catalog edge is bumpable");
        assert_eq!(cands[0].manifest_path, "pnpm-workspace.yaml");
    }

    #[tokio::test]
    async fn force_does_not_propose_a_no_op_bump() {
        // `^0.3.1` with latest 0.3.1: the floor already IS the target, so even
        // --force has nothing to advance. Proposing it would open an empty MR.
        let nodes = vec![node("web", None, vec![npm_edge("@aion/app-boot", "^0.3.1")])];
        let cfg = DiscoverConfig {
            internal_prefixes: vec!["@aion/".into()],
            force: true,
            include_internal: true,
            only_internal: false,
        };
        let cands = find_candidates(&nodes, &ds(&[("@aion/app-boot", "0.3.1")]), &cfg)
            .await
            .unwrap();
        assert!(cands.is_empty(), "no-op bump must not be a candidate");
    }

    #[tokio::test]
    async fn include_internal_with_force_advances_an_admitting_caret() {
        // The aion shape: catalog pins ^0.2.0, registry has 0.2.3. The caret
        // already ADMITS 0.2.3, so only `force` makes it a candidate — that's how
        // the manifest floor (and hence the lockfile) actually moves.
        let nodes = vec![node(
            "web",
            None,
            vec![npm_edge("@aion/http-utils", "^0.2.0")],
        )];
        let ds = ds(&[("@aion/http-utils", "0.2.3")]);

        let admitting = DiscoverConfig {
            internal_prefixes: vec!["@aion/".into()],
            force: false,
            include_internal: true,
            only_internal: false,
        };
        assert!(
            find_candidates(&nodes, &ds, &admitting).await.unwrap().is_empty(),
            "^0.2.0 admits 0.2.3, so without force there is nothing to do"
        );

        let forcing = DiscoverConfig {
            internal_prefixes: vec!["@aion/".into()],
            force: true,
            include_internal: true,
            only_internal: false,
        };
        let cands = find_candidates(&nodes, &ds, &forcing).await.unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].latest, "0.2.3");
        assert_eq!(cands[0].decision, BumpDecision::NeedsBump);
    }

    #[tokio::test]
    async fn non_semver_specs_are_left_alone() {
        let nodes = vec![node(
            "web",
            None,
            vec![
                npm_edge("@s/modules", "workspace:*"),
                npm_edge("next", "catalog:"),
            ],
        )];
        let cands = find_candidates(
            &nodes,
            &ds(&[("next", "16.2.0")]),
            &DiscoverConfig::default(),
        )
        .await
        .unwrap();
        assert!(cands.is_empty());
    }

    #[tokio::test]
    async fn prerelease_latest_is_ignored() {
        let nodes = vec![node("web", None, vec![npm_edge("zod", "^4.4.0")])];
        let cands = find_candidates(
            &nodes,
            &ds(&[("zod", "5.0.0-beta.1")]),
            &DiscoverConfig::default(),
        )
        .await
        .unwrap();
        assert!(cands.is_empty());
    }

    #[tokio::test]
    async fn edges_without_a_datasource_for_their_kind_are_skipped() {
        // A bazel edge with only an npm datasource wired → no candidate, no panic.
        let nodes = vec![node(
            "rules",
            None,
            vec![DepEdge {
                module: "rules_cc".into(),
                current: VersionConstraint::parse_exact("0.1.0"),
                manifest_path: "MODULE.bazel".into(),
                kind: EdgeKind::BazelDep,
            }],
        )];
        let cands = find_candidates(&nodes, &ds(&[("rules_cc", "0.2.0")]), &DiscoverConfig::default())
            .await
            .unwrap();
        assert!(cands.is_empty());
    }
}
