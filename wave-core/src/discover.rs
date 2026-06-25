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
#[must_use]
pub fn is_internal(module: &str, published: &HashSet<String>, cfg: &DiscoverConfig) -> bool {
    published.contains(module)
        || cfg
            .internal_prefixes
            .iter()
            .any(|p| module.starts_with(p.as_str()))
}

/// Scan assembled `nodes` for external-dependency updates, polling each matching
/// [`Datasource`]. Each distinct `(kind, module)` is polled once (results are
/// cached across repos). Edges that are internal, non-semver (`workspace:*`,
/// URLs), prerelease-only, or have no datasource for their ecosystem are
/// skipped. Returns one [`Candidate`] per (repo, dependency) that needs a bump,
/// stably ordered by repo then module.
pub async fn find_candidates(
    nodes: &[RepoNode],
    datasources: &[Box<dyn Datasource>],
    cfg: &DiscoverConfig,
) -> Result<Vec<Candidate>> {
    // Every module published by an enumerated repo is internal by construction.
    let published: HashSet<String> = nodes.iter().filter_map(|n| n.published.clone()).collect();

    // Poll each (kind, module) at most once; `None` = looked up, no usable
    // (stable, semver) latest.
    let mut cache: HashMap<(EdgeKind, String), Option<semver::Version>> = HashMap::new();
    let mut candidates: Vec<Candidate> = Vec::new();

    for node in nodes {
        for edge in &node.edges {
            // Non-semver specs (workspace:*, git/URL, `catalog:`, tags) are not
            // ours to bump from a registry.
            if matches!(edge.current, VersionConstraint::Other(_)) {
                continue;
            }
            if is_internal(&edge.module, &published, cfg) {
                continue;
            }
            let Some(ds) = datasources.iter().find(|d| d.kind() == edge.kind) else {
                continue; // no datasource for this ecosystem (yet)
            };

            let key = (edge.kind, edge.module.clone());
            let latest = match cache.get(&key) {
                Some(v) => v.clone(),
                None => {
                    let v = resolve_latest(ds.as_ref(), &edge.module).await?;
                    cache.insert(key, v.clone());
                    v
                }
            };
            let Some(latest) = latest else { continue };

            let decision = decide(&edge.current, &latest, cfg.force);
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
        };
        let cands = find_candidates(&nodes, &ds(&[("@aion/kernel", "0.5.0")]), &cfg)
            .await
            .unwrap();
        assert!(cands.is_empty());
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
