//! The resumable wave runner.
//!
//! [`materialize`] turns a pure [`WavePlan`] into a durable [`pb::Wave`] with one
//! [`pb::WaveItem`] per affected repo, all `PENDING`. [`WaveRunner::reconcile`]
//! then drives each item through the cascade state machine against a
//! [`forge::Forge`]:
//!
//! ```text
//! PENDING → MR_OPEN → CI_RUNNING → CI_GREEN → MERGING → MERGED → PUBLISHED
//!            (SKIPPED if nothing to bump; FAILED on CI/conflict/forge error)
//! ```
//!
//! Reconcile is **idempotent + resumable**: every forge op tolerates a prior
//! partial attempt, so a daemon can call `reconcile` on a tick (or after a
//! crash) and it picks up exactly where it left off. Each item's deeper-tier
//! readiness is computed from its upstreams' published versions — a tier-2 repo
//! only opens once the tier-1 repo it consumes has actually published, whose
//! version is detected by re-reading the producer's manifest on its default
//! branch after merge.
//!
//! The [`WaveObserver`] seam receives a [`pb::WaveEvent`] for every transition;
//! the tracing exporter (P2) is a sink on top of it. The default [`NullObserver`]
//! drops them.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use chrono::Utc;
use forge::{ChangeState, CiStatus, Forge};

use crate::graph::{repo_key, WavePlan};
use crate::pb;
use crate::provider::ProviderChain;
use crate::{pb_repo_key, to_forge_change, to_forge_repo, to_pb_change};

/// Re-wrap a forge error (its own anyhow instance under Bazel) into ours.
fn forge_err<E: std::fmt::Display>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e:#}")
}

/// Observes every wave state transition. Implementations must be cheap +
/// non-blocking (a durable sink should buffer/append, not do slow I/O inline).
pub trait WaveObserver: Send + Sync {
    fn on_event(&self, event: &pb::WaveEvent);
}

/// Drops every event.
pub struct NullObserver;

impl WaveObserver for NullObserver {
    fn on_event(&self, _event: &pb::WaveEvent) {}
}

/// A deterministic wave id from `(module, target)` + a second-precision stamp.
#[must_use]
pub fn wave_id(module: &str, target_version: &str) -> String {
    format!(
        "wave-{}-v{}-{}",
        slug(module),
        slug(target_version),
        Utc::now().format("%Y%m%dT%H%M%SZ")
    )
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Materialize a pure [`WavePlan`] into a durable [`pb::Wave`] (every item
/// `PENDING`). The wave + branch ids are deterministic so a re-materialize over
/// the same plan is stable.
#[must_use]
pub fn materialize(plan: &WavePlan, force_bump_satisfied: bool) -> pb::Wave {
    let now = Utc::now().to_rfc3339();
    let id = wave_id(&plan.root_module, &plan.target_version);
    let branch = format!("wave/{id}");
    let items = plan
        .items
        .iter()
        .map(|pi| pb::WaveItem {
            repo: Some(crate::to_pb_repo(&pi.repo)),
            published_module: pi.published.clone().unwrap_or_default(),
            manifest_path: pi.manifest_path.clone(),
            tier: pi.tier,
            upstream_modules: pi.upstream_modules.clone(),
            state: pb::WaveItemState::Pending as i32,
            branch: branch.clone(),
            change: None,
            upstream_targets: HashMap::new(),
            baseline_version: String::new(),
            published_version: String::new(),
            error: String::new(),
            updated_at: now.clone(),
        })
        .collect();

    pb::Wave {
        id,
        root_module: plan.root_module.clone(),
        root_repo: plan.root_repo.as_ref().map(crate::to_pb_repo),
        target_version: plan.target_version.clone(),
        state: pb::WaveState::Open as i32,
        items,
        cycle: plan.cycle.clone(),
        force_bump_satisfied,
        created_at: now.clone(),
        completed_at: String::new(),
    }
}

/// The next forge action an item warrants, derived purely from wave state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemAction {
    /// An upstream hasn't published yet — nothing to do.
    Wait,
    /// Open the bump: resolved `module → target version` to write.
    Open(BTreeMap<String, String>),
    /// Poll the open change's CI / merge state.
    Poll,
    /// Merge the green change.
    Merge,
    /// Detect this repo's own publish.
    DetectPublish,
    /// Terminal — nothing more to do.
    Done,
}

fn state_of(item: &pb::WaveItem) -> pb::WaveItemState {
    pb::WaveItemState::try_from(item.state).unwrap_or(pb::WaveItemState::Unspecified)
}

/// Has this item resolved a final published version downstream can depend on?
fn is_resolved(item: &pb::WaveItem) -> bool {
    matches!(
        state_of(item),
        pb::WaveItemState::Published | pb::WaveItemState::Skipped
    )
}

/// Resolve each upstream module's target version. `None` if any upstream hasn't
/// resolved yet (the item must wait). The root module resolves to the wave
/// target; other modules resolve to their producer item's published version.
fn resolve_targets(wave: &pb::Wave, item: &pb::WaveItem) -> Option<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for m in &item.upstream_modules {
        if *m == wave.root_module {
            out.insert(m.clone(), wave.target_version.clone());
            continue;
        }
        let producer = wave.items.iter().find(|it| it.published_module == *m);
        match producer {
            Some(p) if is_resolved(p) && !p.published_version.is_empty() => {
                out.insert(m.clone(), p.published_version.clone());
            }
            // No producer in the wave, or it hasn't published — wait.
            _ => return None,
        }
    }
    Some(out)
}

/// The next action for `item`, given the whole `wave`. Pure + total.
#[must_use]
pub fn next_action(wave: &pb::Wave, item: &pb::WaveItem) -> ItemAction {
    use pb::WaveItemState as S;
    match state_of(item) {
        S::Pending => resolve_targets(wave, item).map_or(ItemAction::Wait, ItemAction::Open),
        S::MrOpen | S::CiRunning => ItemAction::Poll,
        S::CiGreen | S::Merging => ItemAction::Merge,
        S::Merged => ItemAction::DetectPublish,
        S::Published | S::Skipped | S::Failed | S::Unspecified => ItemAction::Done,
    }
}

/// Drives wave items through the cascade against a borrowed forge + provider
/// chain, emitting events to an observer.
pub struct WaveRunner<'a, F: Forge + ?Sized, O: WaveObserver + ?Sized> {
    forge: &'a F,
    chain: &'a ProviderChain,
    observer: &'a O,
}

impl<'a, F: Forge + ?Sized, O: WaveObserver + ?Sized> WaveRunner<'a, F, O> {
    #[must_use]
    pub fn new(forge: &'a F, chain: &'a ProviderChain, observer: &'a O) -> Self {
        Self {
            forge,
            chain,
            observer,
        }
    }

    /// Advance every ready item until a full pass makes no progress, then settle
    /// the wave's own state. Idempotent + resumable.
    pub async fn reconcile(&self, wave: &mut pb::Wave) -> Result<()> {
        loop {
            let mut progressed = false;
            for idx in 0..wave.items.len() {
                let action = next_action(wave, &wave.items[idx]);
                progressed |= self.step(wave, idx, action).await?;
            }
            if !progressed {
                break;
            }
        }
        self.finalize(wave);
        Ok(())
    }

    async fn step(&self, wave: &mut pb::Wave, idx: usize, action: ItemAction) -> Result<bool> {
        match action {
            ItemAction::Wait | ItemAction::Done => Ok(false),
            ItemAction::Open(targets) => self.open(wave, idx, &targets).await,
            ItemAction::Poll => self.poll(wave, idx).await,
            ItemAction::Merge => self.do_merge(wave, idx).await,
            ItemAction::DetectPublish => self.detect_publish(wave, idx).await,
        }
    }

    async fn open(
        &self,
        wave: &mut pb::Wave,
        idx: usize,
        targets: &BTreeMap<String, String>,
    ) -> Result<bool> {
        let wave_id = wave.id.clone();
        let item = wave.items[idx].clone();
        let repo_pb = item.repo.clone().context("wave item missing repo")?;
        let repo = to_forge_repo(&repo_pb);
        let manifest_path = item.manifest_path.clone();
        let branch = item.branch.clone();
        let provider = self
            .chain
            .for_manifest(&manifest_path)
            .with_context(|| format!("no provider for manifest {manifest_path}"))?;

        let default_branch = self.forge.default_branch(&repo).await.map_err(forge_err)?;
        let blob = self
            .forge
            .read_file(&repo, &manifest_path, "")
            .await
            .map_err(forge_err)?
            .with_context(|| format!("{manifest_path} not found on {}", repo_key(&repo)))?;
        let original = blob.content;

        // Apply every resolved upstream bump to the manifest text.
        let mut text = original.clone();
        let mut any_changed = false;
        for (module, target) in targets {
            let (next, changed) = provider.bump(&text, module, target);
            if changed {
                text = next;
                any_changed = true;
            }
        }

        // Record what this item resolved its upstreams to, for status/tracing.
        wave.items[idx].upstream_targets = targets.clone().into_iter().collect();

        if !any_changed {
            // Already satisfied — no change to open. Surface this repo's current
            // published version so downstream items resolve against it.
            let pubver = provider.published_version(&original).unwrap_or_default();
            wave.items[idx].published_version = pubver;
            self.transition(
                wave,
                idx,
                pb::WaveItemState::Skipped,
                "constraint already satisfied".into(),
            );
            return Ok(true);
        }

        // The publish-detection baseline: this repo's own version before merge.
        let baseline = provider.published_version(&original).unwrap_or_default();

        // Branch → commit → open change → enable auto-merge (each idempotent).
        self.forge
            .create_branch(&repo, &branch, &default_branch)
            .await
            .map_err(forge_err)?;
        let summary = summarize(targets);
        let message = format!("Bump {summary}");
        self.forge
            .commit_file(&repo, &branch, &manifest_path, &text, &blob.blob_sha, &message)
            .await
            .map_err(forge_err)?;
        let title = format!("Bump {summary}");
        let body = format!("Wave `{wave_id}`\n\nBumps {summary}.\n");
        let opened = self
            .forge
            .open_change(&repo, &branch, &default_branch, &title, &body, true)
            .await
            .map_err(forge_err)?;
        let enabled = self
            .forge
            .enable_auto_merge(&repo, &opened.change)
            .await
            .unwrap_or(false);

        let url = opened.change.url.clone();
        wave.items[idx].baseline_version = baseline;
        wave.items[idx].change = Some(to_pb_change(&opened.change));
        // Auto-merge enabled → the forge merges on green (CI_RUNNING). Otherwise
        // we'll merge it ourselves after CI goes green (MR_OPEN).
        let next = if enabled {
            pb::WaveItemState::CiRunning
        } else {
            pb::WaveItemState::MrOpen
        };
        self.transition(wave, idx, next, url);
        Ok(true)
    }

    async fn poll(&self, wave: &mut pb::Wave, idx: usize) -> Result<bool> {
        let item = wave.items[idx].clone();
        let repo = to_forge_repo(&item.repo.clone().context("wave item missing repo")?);
        let change =
            to_forge_change(&item.change.clone().context("polling an item with no change")?);

        // Auto-merge may have already merged it.
        match self.forge.change_state(&repo, &change).await.map_err(forge_err)? {
            ChangeState::Merged => {
                self.transition(wave, idx, pb::WaveItemState::Merged, "merged".into());
                return Ok(true);
            }
            ChangeState::Closed => {
                self.transition(
                    wave,
                    idx,
                    pb::WaveItemState::Failed,
                    "change closed without merging".into(),
                );
                return Ok(true);
            }
            _ => {}
        }

        let ps = self
            .forge
            .pipeline_status(&repo, &change)
            .await
            .map_err(forge_err)?;
        match ps.status {
            CiStatus::Success => {
                self.transition(wave, idx, pb::WaveItemState::CiGreen, ps.url);
                Ok(true)
            }
            CiStatus::Failed | CiStatus::Canceled => {
                self.transition(
                    wave,
                    idx,
                    pb::WaveItemState::Failed,
                    format!("pipeline {:?}", ps.status),
                );
                Ok(true)
            }
            // Pending / running / none → no progress this tick.
            _ => Ok(false),
        }
    }

    async fn do_merge(&self, wave: &mut pb::Wave, idx: usize) -> Result<bool> {
        let item = wave.items[idx].clone();
        let repo = to_forge_repo(&item.repo.clone().context("wave item missing repo")?);
        let change =
            to_forge_change(&item.change.clone().context("merging an item with no change")?);

        // Auto-merge could have fired between poll and now.
        if matches!(
            self.forge.change_state(&repo, &change).await.map_err(forge_err)?,
            ChangeState::Merged
        ) {
            self.transition(wave, idx, pb::WaveItemState::Merged, "merged".into());
            return Ok(true);
        }
        let sha = self.forge.merge(&repo, &change).await.map_err(forge_err)?;
        self.transition(wave, idx, pb::WaveItemState::Merged, sha);
        Ok(true)
    }

    async fn detect_publish(&self, wave: &mut pb::Wave, idx: usize) -> Result<bool> {
        let item = wave.items[idx].clone();

        // An app publishes nothing — merging is terminal success.
        if item.published_module.is_empty() {
            self.transition(
                wave,
                idx,
                pb::WaveItemState::Published,
                "merged (publishes nothing)".into(),
            );
            return Ok(true);
        }

        let repo = to_forge_repo(&item.repo.clone().context("wave item missing repo")?);
        let provider = self
            .chain
            .for_manifest(&item.manifest_path)
            .with_context(|| format!("no provider for manifest {}", item.manifest_path))?;
        let Some(blob) = self
            .forge
            .read_file(&repo, &item.manifest_path, "")
            .await
            .map_err(forge_err)?
        else {
            return Ok(false);
        };
        let current = provider.published_version(&blob.content).unwrap_or_default();
        if !current.is_empty() && current != item.baseline_version {
            wave.items[idx].published_version = current.clone();
            self.transition(
                wave,
                idx,
                pb::WaveItemState::Published,
                format!("published {current}"),
            );
            Ok(true)
        } else {
            // Release pipeline hasn't bumped the published version yet — re-check
            // on the next reconcile tick.
            Ok(false)
        }
    }

    /// Apply a state transition + emit the event.
    fn transition(&self, wave: &mut pb::Wave, idx: usize, to: pb::WaveItemState, detail: String) {
        let now = Utc::now().to_rfc3339();
        let wave_id = wave.id.clone();
        let item = &mut wave.items[idx];
        let from = item.state;
        let item_key = item.repo.as_ref().map(pb_repo_key).unwrap_or_default();
        if matches!(to, pb::WaveItemState::Failed) {
            item.error = detail.clone();
        }
        item.state = to as i32;
        item.updated_at = now.clone();
        self.observer.on_event(&pb::WaveEvent {
            wave_id,
            item_key,
            from_state: from,
            to_state: to as i32,
            detail,
            at: now,
        });
    }

    /// Settle the wave's own state from its items.
    fn finalize(&self, wave: &mut pb::Wave) {
        use pb::WaveItemState as S;
        let terminal = |it: &pb::WaveItem| {
            matches!(state_of(it), S::Published | S::Skipped | S::Failed)
        };
        let any_failed = wave.items.iter().any(|it| matches!(state_of(it), S::Failed));
        if wave.items.is_empty() || wave.items.iter().all(terminal) {
            wave.state = if any_failed {
                pb::WaveState::Aborted as i32
            } else {
                pb::WaveState::Completed as i32
            };
            if wave.completed_at.is_empty() {
                wave.completed_at = Utc::now().to_rfc3339();
            }
        } else {
            wave.state = pb::WaveState::Merging as i32;
        }
    }
}

/// `"a to 1.0, b to 2.0"` — for change titles/messages.
fn summarize(targets: &BTreeMap<String, String>) -> String {
    targets
        .iter()
        .map(|(m, v)| format!("{m} to {v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::propose;
    use crate::provider::{GraphProvider, NpmProvider};
    use async_trait::async_trait;
    use forge::{
        BranchOutcome, ChangeRef, FileBlob, Forge as ForgeTrait, ForgeKind, OpenedChange,
        PipelineStatus, RepoRef,
    };
    use std::sync::Mutex;

    fn repo(name: &str) -> RepoRef {
        RepoRef {
            forge: ForgeKind::Gitlab as i32,
            host: "gitlab.savvifi.com".into(),
            owner: "studio".into(),
            name: name.into(),
        }
    }

    // ── pure decision tests ────────────────────────────────────────────────

    #[test]
    fn pending_waits_until_upstream_publishes() {
        let producer = pb::WaveItem {
            repo: Some(crate::to_pb_repo(&repo("foundation"))),
            published_module: "@s/foundation".into(),
            state: pb::WaveItemState::Merged as i32, // not yet published
            ..Default::default()
        };
        let consumer = pb::WaveItem {
            repo: Some(crate::to_pb_repo(&repo("web"))),
            upstream_modules: vec!["@s/foundation".into()],
            state: pb::WaveItemState::Pending as i32,
            ..Default::default()
        };
        let wave = pb::Wave {
            root_module: "@s/modules".into(),
            target_version: "0.1.1".into(),
            items: vec![producer, consumer.clone()],
            ..Default::default()
        };
        assert_eq!(next_action(&wave, &consumer), ItemAction::Wait);
    }

    #[test]
    fn pending_opens_once_upstream_published() {
        let producer = pb::WaveItem {
            repo: Some(crate::to_pb_repo(&repo("foundation"))),
            published_module: "@s/foundation".into(),
            state: pb::WaveItemState::Published as i32,
            published_version: "0.1.1".into(),
            ..Default::default()
        };
        let consumer = pb::WaveItem {
            repo: Some(crate::to_pb_repo(&repo("web"))),
            upstream_modules: vec!["@s/foundation".into()],
            state: pb::WaveItemState::Pending as i32,
            ..Default::default()
        };
        let wave = pb::Wave {
            root_module: "@s/modules".into(),
            target_version: "0.1.1".into(),
            items: vec![producer, consumer.clone()],
            ..Default::default()
        };
        match next_action(&wave, &consumer) {
            ItemAction::Open(t) => assert_eq!(t.get("@s/foundation").map(String::as_str), Some("0.1.1")),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    // ── mock forge: a fully in-memory cascade ───────────────────────────────

    #[derive(Default)]
    struct RepoSim {
        // path → content on the default branch.
        default_files: HashMap<String, String>,
        // (branch, path) → content.
        branch_files: HashMap<(String, String), String>,
        changes: Vec<MockChange>,
        // (old_version, new_version) applied to the repo's own manifest on merge,
        // simulating the release pipeline. None = an app that publishes nothing.
        release: Option<(String, String)>,
    }

    struct MockChange {
        number: u64,
        branch: String,
        auto_merge: bool,
        merged: bool,
    }

    #[derive(Default)]
    struct MockForge {
        repos: Mutex<HashMap<String, RepoSim>>,
    }

    impl MockForge {
        fn seed(&self, name: &str, path: &str, content: &str, release: Option<(&str, &str)>) {
            let mut sim = RepoSim::default();
            sim.default_files.insert(path.into(), content.into());
            sim.release = release.map(|(a, b)| (a.to_string(), b.to_string()));
            self.repos.lock().unwrap().insert(repo_key(&repo(name)), sim);
        }

        fn default_file(&self, name: &str, path: &str) -> String {
            self.repos.lock().unwrap()[&repo_key(&repo(name))].default_files[path].clone()
        }

        // Apply a branch's committed files to the default branch + run the
        // simulated release (bump the repo's own version).
        fn apply_merge(sim: &mut RepoSim, branch: &str) {
            let updates: Vec<(String, String)> = sim
                .branch_files
                .iter()
                .filter(|((b, _), _)| b == branch)
                .map(|((_, p), c)| (p.clone(), c.clone()))
                .collect();
            for (path, content) in updates {
                sim.default_files.insert(path, content);
            }
            if let Some((old, new)) = sim.release.clone() {
                // The seed manifests are compact JSON (`"version":"x"`), so match
                // that form when simulating the release pipeline's self-bump.
                for content in sim.default_files.values_mut() {
                    *content = content.replace(
                        &format!("\"version\":\"{old}\""),
                        &format!("\"version\":\"{new}\""),
                    );
                }
            }
        }
    }

    #[async_trait]
    impl ForgeTrait for MockForge {
        fn kind(&self) -> ForgeKind {
            ForgeKind::Gitlab
        }
        async fn default_branch(&self, _repo: &RepoRef) -> forge::ForgeResult<String> {
            Ok("main".into())
        }
        async fn read_file(
            &self,
            repo: &RepoRef,
            path: &str,
            r#ref: &str,
        ) -> forge::ForgeResult<Option<FileBlob>> {
            let repos = self.repos.lock().unwrap();
            let sim = repos.get(&repo_key(repo)).ok_or_else(|| forge::ForgeError::msg("unknown repo"))?;
            let content = if r#ref.is_empty() || r#ref == "main" {
                sim.default_files.get(path).cloned()
            } else {
                sim.branch_files
                    .get(&(r#ref.to_string(), path.to_string()))
                    .or_else(|| sim.default_files.get(path))
                    .cloned()
            };
            Ok(content.map(|c| FileBlob {
                path: path.into(),
                content: c,
                blob_sha: "mock-sha".into(),
            }))
        }
        async fn create_branch(
            &self,
            repo: &RepoRef,
            name: &str,
            _from_ref: &str,
        ) -> forge::ForgeResult<BranchOutcome> {
            let mut repos = self.repos.lock().unwrap();
            let sim = repos.get_mut(&repo_key(repo)).ok_or_else(|| forge::ForgeError::msg("unknown repo"))?;
            // Seed the branch with the default-branch files.
            let seed: Vec<(String, String)> = sim
                .default_files
                .iter()
                .map(|(p, c)| (p.clone(), c.clone()))
                .collect();
            for (p, c) in seed {
                sim.branch_files
                    .entry((name.to_string(), p))
                    .or_insert(c);
            }
            Ok(BranchOutcome {
                created: true,
                already_existed: false,
            })
        }
        async fn commit_file(
            &self,
            repo: &RepoRef,
            branch: &str,
            path: &str,
            content: &str,
            _blob_sha: &str,
            _message: &str,
        ) -> forge::ForgeResult<String> {
            let mut repos = self.repos.lock().unwrap();
            let sim = repos.get_mut(&repo_key(repo)).ok_or_else(|| forge::ForgeError::msg("unknown repo"))?;
            sim.branch_files
                .insert((branch.to_string(), path.to_string()), content.to_string());
            Ok("commit-sha".into())
        }
        async fn open_change(
            &self,
            repo: &RepoRef,
            head: &str,
            _base: &str,
            _title: &str,
            _body: &str,
            _remove_source_branch: bool,
        ) -> forge::ForgeResult<OpenedChange> {
            let mut repos = self.repos.lock().unwrap();
            let sim = repos.get_mut(&repo_key(repo)).ok_or_else(|| forge::ForgeError::msg("unknown repo"))?;
            let number = sim.changes.len() as u64 + 1;
            sim.changes.push(MockChange {
                number,
                branch: head.to_string(),
                auto_merge: false,
                merged: false,
            });
            Ok(OpenedChange {
                change: ChangeRef {
                    number,
                    url: format!("https://forge/{}/-/merge_requests/{number}", repo.name),
                    branch: head.to_string(),
                },
                already_existed: false,
            })
        }
        async fn enable_auto_merge(&self, repo: &RepoRef, change: &ChangeRef) -> forge::ForgeResult<bool> {
            let mut repos = self.repos.lock().unwrap();
            let sim = repos.get_mut(&repo_key(repo)).ok_or_else(|| forge::ForgeError::msg("unknown repo"))?;
            if let Some(c) = sim.changes.iter_mut().find(|c| c.number == change.number) {
                c.auto_merge = true;
            }
            Ok(true)
        }
        async fn pipeline_status(
            &self,
            _repo: &RepoRef,
            _change: &ChangeRef,
        ) -> forge::ForgeResult<PipelineStatus> {
            // CI is instantly green in the simulation.
            Ok(PipelineStatus {
                status: CiStatus::Success,
                pipeline_id: "1".into(),
                url: "https://forge/pipeline/1".into(),
            })
        }
        async fn merge(&self, repo: &RepoRef, change: &ChangeRef) -> forge::ForgeResult<String> {
            let mut repos = self.repos.lock().unwrap();
            let sim = repos.get_mut(&repo_key(repo)).ok_or_else(|| forge::ForgeError::msg("unknown repo"))?;
            let branch = sim
                .changes
                .iter()
                .find(|c| c.number == change.number)
                .map(|c| c.branch.clone())
                .ok_or_else(|| forge::ForgeError::msg("unknown change"))?;
            MockForge::apply_merge(sim, &branch);
            if let Some(c) = sim.changes.iter_mut().find(|c| c.number == change.number) {
                c.merged = true;
            }
            Ok("merge-sha".into())
        }
        async fn change_state(&self, repo: &RepoRef, change: &ChangeRef) -> forge::ForgeResult<ChangeState> {
            let mut repos = self.repos.lock().unwrap();
            let sim = repos.get_mut(&repo_key(repo)).ok_or_else(|| forge::ForgeError::msg("unknown repo"))?;
            let (merged, auto, branch) = {
                let c = sim
                    .changes
                    .iter()
                    .find(|c| c.number == change.number)
                    .ok_or_else(|| forge::ForgeError::msg("unknown change"))?;
                (c.merged, c.auto_merge, c.branch.clone())
            };
            if merged {
                return Ok(ChangeState::Merged);
            }
            // Merge-when-pipeline-succeeds: CI is always green, so an auto-merge
            // change merges on the first state poll.
            if auto {
                MockForge::apply_merge(sim, &branch);
                if let Some(c) = sim.changes.iter_mut().find(|c| c.number == change.number) {
                    c.merged = true;
                }
                return Ok(ChangeState::Merged);
            }
            Ok(ChangeState::Open)
        }
    }

    fn npm_node(name: &str, json: &str, publishes: bool) -> crate::graph::RepoNode {
        let p = NpmProvider::new();
        crate::graph::RepoNode {
            repo: repo(name),
            published: if publishes { p.published_name(json) } else { None },
            edges: p.parse_edges(json).unwrap_or_default(),
        }
    }

    #[tokio::test]
    async fn cascade_drives_two_tiers_to_completion() {
        // modules (root) → foundation (publishes) → web (app).
        let modules = r#"{"name":"@s/modules","version":"0.1.1"}"#;
        let foundation =
            r#"{"name":"@s/foundation","version":"0.1.0","dependencies":{"@s/modules":"^0.1.0"}}"#;
        let web =
            r#"{"name":"@s/web","version":"0.0.0","dependencies":{"@s/foundation":"^0.1.0"}}"#;

        let nodes = vec![
            npm_node("modules", modules, true),
            npm_node("foundation", foundation, true),
            npm_node("web", web, false), // web is an app: publishes nothing
        ];
        let plan = propose(nodes, "@s/modules", "0.1.1", false);
        let mut wave = materialize(&plan, false);

        let forge = MockForge::default();
        forge.seed("modules", "package.json", modules, None);
        forge.seed("foundation", "package.json", foundation, Some(("0.1.0", "0.1.1")));
        forge.seed("web", "package.json", web, None);

        let chain = ProviderChain::default_chain();
        let observer = NullObserver;
        let runner = WaveRunner::new(&forge, &chain, &observer);
        runner.reconcile(&mut wave).await.unwrap();

        let item = |name: &str| {
            wave.items
                .iter()
                .find(|i| i.repo.as_ref().unwrap().name == name)
                .unwrap()
        };
        assert_eq!(item("foundation").state, pb::WaveItemState::Published as i32);
        assert_eq!(item("foundation").published_version, "0.1.1");
        assert!(item("foundation").change.is_some());
        assert_eq!(item("web").state, pb::WaveItemState::Published as i32);
        assert_eq!(wave.state, pb::WaveState::Completed as i32);

        // The forge's live manifests reflect the cascade: foundation bumped its
        // dep on modules, web bumped its dep on foundation. The bump preserves
        // the source's (compact) formatting.
        assert!(forge
            .default_file("foundation", "package.json")
            .contains(r#""@s/modules":"^0.1.1""#));
        assert!(forge
            .default_file("web", "package.json")
            .contains(r#""@s/foundation":"^0.1.1""#));
    }
}
