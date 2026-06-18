//! Cross-repo DAG assembly + the tiered cascade plan.
//!
//! Given each repo's `{published name, dependency edges}`, build the
//! `consumer → producer` graph, compute the set affected by bumping one module,
//! assign each affected repo a **tier** (its longest-path depth from the bump),
//! detect cycles, and emit a topologically-ordered [`WavePlan`].

use std::collections::{HashMap, HashSet, VecDeque};

use forge::RepoRef;

use crate::edge::{decide, BumpDecision, DepEdge};

/// One repo's contribution to the graph.
pub struct RepoNode {
    pub repo: RepoRef,
    /// The module/package this repo publishes, if any (apps publish nothing).
    pub published: Option<String>,
    pub edges: Vec<DepEdge>,
}

/// Stable key for a repo across forges/hosts.
#[must_use]
pub fn repo_key(r: &RepoRef) -> String {
    format!("{}|{}/{}", r.host, r.owner, r.name)
}

/// One repo's place in a proposed cascade.
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub repo: RepoRef,
    /// The module/package this repo publishes, if any (apps publish nothing).
    pub published: Option<String>,
    /// Cascade depth: 1 = direct consumer of the bumped module, 2 = its
    /// consumer, … (longest path from the bump).
    pub tier: u32,
    /// The first-party modules this repo consumes that participate in the wave
    /// (the bumped module for tier-1; upstream repos' modules for deeper tiers).
    pub upstream_modules: Vec<String>,
    /// The manifest the wave-participating edges live in (e.g. `"MODULE.bazel"`,
    /// `"package.json"`) — the file the bump rewrites.
    pub manifest_path: String,
    /// For a direct consumer of the bumped module: whether it needs a bump to
    /// the target. `None` for deeper tiers (their target is the upstream's
    /// not-yet-known published version, resolved at run time).
    pub decision: Option<BumpDecision>,
}

/// A proposed cascade: the affected repos in dependency order.
#[derive(Debug, Clone)]
pub struct WavePlan {
    pub root_module: String,
    pub root_repo: Option<RepoRef>,
    pub target_version: String,
    /// Affected repos, ascending by tier then key.
    pub items: Vec<PlanItem>,
    /// Repo keys that form a dependency cycle (couldn't be ordered).
    pub cycle: Vec<String>,
}

/// Compute the tiered cascade for bumping `root_module` to `target_version`.
/// `bump_satisfied` forces bumps even where a caret/range already admits the
/// target (to advance pinned lockfile floors).
#[must_use]
pub fn propose(
    nodes: Vec<RepoNode>,
    root_module: &str,
    target_version: &str,
    bump_satisfied: bool,
) -> WavePlan {
    // Index nodes + the module → producing-repo map.
    let mut by_key: HashMap<String, RepoNode> = HashMap::new();
    let mut producer_of: HashMap<String, String> = HashMap::new();
    for n in nodes {
        let k = repo_key(&n.repo);
        if let Some(name) = &n.published {
            producer_of.insert(name.clone(), k.clone());
        }
        by_key.insert(k, n);
    }

    // First-party adjacency: consumer → producers, producer → consumers.
    let mut producers: HashMap<String, Vec<String>> = HashMap::new();
    let mut consumers: HashMap<String, Vec<String>> = HashMap::new();
    for (k, n) in &by_key {
        for e in &n.edges {
            if let Some(pk) = producer_of.get(&e.module) {
                if pk == k {
                    continue; // self-dep
                }
                producers.entry(k.clone()).or_default().push(pk.clone());
                consumers.entry(pk.clone()).or_default().push(k.clone());
            }
        }
    }

    let root_repo = producer_of
        .get(root_module)
        .and_then(|k| by_key.get(k))
        .map(|n| n.repo.clone());

    // Direct consumers of the bumped module = repos with an edge on it.
    let direct: Vec<String> = by_key
        .iter()
        .filter(|(_, n)| n.edges.iter().any(|e| e.module == root_module))
        .map(|(k, _)| k.clone())
        .collect();

    // Affected = downstream closure of `direct` (BFS over consumers). The root
    // repo itself is the source (tier 0), not part of the affected work set.
    let mut affected: HashSet<String> = direct.iter().cloned().collect();
    let mut q: VecDeque<String> = direct.iter().cloned().collect();
    while let Some(k) = q.pop_front() {
        if let Some(cs) = consumers.get(&k) {
            for c in cs {
                if affected.insert(c.clone()) {
                    q.push_back(c.clone());
                }
            }
        }
    }

    // In-degree within the affected subgraph (affected producers only).
    let mut indeg: HashMap<String, usize> = affected.iter().map(|k| (k.clone(), 0)).collect();
    for k in &affected {
        if let Some(ps) = producers.get(k) {
            for p in ps {
                if affected.contains(p) {
                    *indeg.get_mut(k).expect("affected key") += 1;
                }
            }
        }
    }

    // Kahn topo + longest-path tier (1 + max affected-producer tier).
    let mut tier: HashMap<String, u32> = HashMap::new();
    let mut ready: VecDeque<String> = affected
        .iter()
        .filter(|k| indeg[*k] == 0)
        .cloned()
        .collect();
    while let Some(k) = ready.pop_front() {
        let t = producers
            .get(&k)
            .map(|ps| {
                ps.iter()
                    .filter(|p| affected.contains(*p))
                    .filter_map(|p| tier.get(p))
                    .copied()
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
            + 1;
        tier.insert(k.clone(), t);
        if let Some(cs) = consumers.get(&k) {
            for c in cs {
                if affected.contains(c) {
                    let d = indeg.get_mut(c).expect("consumer indeg");
                    *d -= 1;
                    if *d == 0 {
                        ready.push_back(c.clone());
                    }
                }
            }
        }
    }

    let cycle: Vec<String> = affected
        .iter()
        .filter(|k| !tier.contains_key(*k))
        .cloned()
        .collect();

    let target = semver::Version::parse(target_version.trim()).ok();

    // Build the plan items for every affected, ordered repo.
    let mut items: Vec<PlanItem> = Vec::new();
    for (k, &t) in &tier {
        let node = &by_key[k];
        let mut upstream: Vec<String> = Vec::new();
        // The manifest the wave-participating edges live in (the file the bump
        // rewrites). All of a repo's first-party edges share one manifest kind.
        let mut manifest_path = String::new();
        for e in &node.edges {
            let first_party = producer_of
                .get(&e.module)
                .is_some_and(|pk| affected.contains(pk));
            if e.module == root_module || first_party {
                upstream.push(e.module.clone());
                if manifest_path.is_empty() {
                    manifest_path.clone_from(&e.manifest_path);
                }
            }
        }
        upstream.sort();
        upstream.dedup();

        // Decision only for direct consumers of the root module vs the target.
        let decision = node
            .edges
            .iter()
            .find(|e| e.module == root_module)
            .and_then(|e| target.as_ref().map(|tv| decide(&e.current, tv, bump_satisfied)));

        items.push(PlanItem {
            repo: node.repo.clone(),
            published: node.published.clone(),
            tier: t,
            upstream_modules: upstream,
            manifest_path,
            decision,
        });
    }
    items.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| repo_key(&a.repo).cmp(&repo_key(&b.repo))));

    WavePlan {
        root_module: root_module.to_string(),
        root_repo,
        target_version: target_version.to_string(),
        items,
        cycle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::{EdgeKind, VersionConstraint};

    fn repo(name: &str) -> RepoRef {
        RepoRef {
            forge: forge::ForgeKind::Gitlab as i32,
            host: "gitlab.savvifi.com".into(),
            owner: "studio".into(),
            name: name.into(),
        }
    }

    fn node(name: &str, publishes: Option<&str>, deps: &[(&str, &str)]) -> RepoNode {
        RepoNode {
            repo: repo(name),
            published: publishes.map(str::to_string),
            edges: deps
                .iter()
                .map(|(m, v)| DepEdge {
                    module: (*m).to_string(),
                    current: VersionConstraint::parse_exact(v),
                    manifest_path: "MODULE.bazel".into(),
                    kind: EdgeKind::BazelDep,
                })
                .collect(),
        }
    }

    #[test]
    fn tiers_follow_the_dag_with_longest_path() {
        // modules → foundation → api → web; web also depends on foundation.
        let nodes = vec![
            node("modules", Some("modules"), &[]),
            node("foundation", Some("foundation"), &[("modules", "0.1.0")]),
            node("api", Some("api"), &[("foundation", "0.1.0")]),
            node(
                "web",
                None,
                &[("api", "0.1.0"), ("foundation", "0.1.0")],
            ),
        ];
        let plan = propose(nodes, "modules", "0.1.1", false);
        assert!(plan.cycle.is_empty());
        let tier_of = |n: &str| {
            plan.items
                .iter()
                .find(|i| i.repo.name == n)
                .map(|i| i.tier)
                .unwrap()
        };
        assert_eq!(tier_of("foundation"), 1);
        assert_eq!(tier_of("api"), 2);
        // web depends on api(2) AND foundation(1) → longest path = 3.
        assert_eq!(tier_of("web"), 3);
        // modules is the source (tier 0), not in the affected work set.
        assert!(plan.items.iter().all(|i| i.repo.name != "modules"));
        // items are ordered by tier.
        let tiers: Vec<u32> = plan.items.iter().map(|i| i.tier).collect();
        let mut sorted = tiers.clone();
        sorted.sort_unstable();
        assert_eq!(tiers, sorted);
        // tier-1 foundation has an exact pin 0.1.0 < 0.1.1 → needs a bump.
        let foundation = plan.items.iter().find(|i| i.repo.name == "foundation").unwrap();
        assert_eq!(foundation.decision, Some(BumpDecision::NeedsBump));
    }

    #[test]
    fn cycle_is_surfaced_not_hung() {
        // a ↔ b mutually depend → cycle, plus c depends on a as the trigger.
        let nodes = vec![
            node("a", Some("a"), &[("b", "0.1.0"), ("seed", "0.1.0")]),
            node("b", Some("b"), &[("a", "0.1.0")]),
        ];
        let plan = propose(nodes, "seed", "0.2.0", false);
        // a is the direct consumer of `seed`; a↔b cycle means neither orders.
        assert!(!plan.cycle.is_empty());
    }
}
