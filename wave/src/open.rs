//! `wave discover --open` — turn discovery candidates into ONE change per repo.
//!
//! This is the *flat* actuation path, deliberately not the cascade: it bumps a
//! repo's own pins and stops. There is no tier ordering, no publish detection and
//! no waiting, because nothing downstream depends on the result — the repos this
//! serves pin every dependency themselves (a pnpm catalog is exactly that: one
//! flat, repo-wide version map), so a bump needs no upstream to republish first.
//! Multi-repo propagation is [`runner::WaveRunner`](wave_core::WaveRunner)'s job.
//!
//! The branch name is STABLE, which makes the whole path idempotent against a
//! periodic schedule: `create_branch` and `open_change` both no-op onto what
//! already exists, so a re-run refreshes the same MR with newer versions rather
//! than opening a second one. Manifests are read back from the wave branch (not
//! the default branch) so a re-run builds on prior commits instead of reverting
//! them.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use forge::{Forge, RepoRef};
use wave_core::{repo_key, Candidate, ProviderChain};

/// What `--open` did for one repo.
pub struct OpenOutcome {
    pub repo: RepoRef,
    pub url: String,
    /// The change already existed and was refreshed rather than created.
    pub already_existed: bool,
    /// `(module, target)` actually rewritten, for the report.
    pub bumped: Vec<(String, String)>,
}

/// Open (or refresh) one change per repo carrying every candidate bump for it.
/// `auto_merge` arms merge-when-pipeline-succeeds; leave it off to hold the
/// change for review.
pub async fn open_candidates(
    forge: &dyn Forge,
    chain: &ProviderChain,
    candidates: &[Candidate],
    branch: &str,
    auto_merge: bool,
) -> Result<Vec<OpenOutcome>> {
    // Group by repo, then by the manifest each bump rewrites — one commit per
    // manifest, one change per repo. BTreeMap keeps the output stable.
    let mut by_repo: BTreeMap<String, (RepoRef, BTreeMap<String, Vec<&Candidate>>)> =
        BTreeMap::new();
    for c in candidates {
        let entry = by_repo
            .entry(repo_key(&c.repo))
            .or_insert_with(|| (c.repo.clone(), BTreeMap::new()));
        entry
            .1
            .entry(c.manifest_path.clone())
            .or_default()
            .push(c);
    }

    let mut out = Vec::new();
    for (_, (repo, by_manifest)) in by_repo {
        if let Some(outcome) = open_one(forge, chain, &repo, &by_manifest, branch, auto_merge).await?
        {
            out.push(outcome);
        }
    }
    Ok(out)
}

async fn open_one(
    forge: &dyn Forge,
    chain: &ProviderChain,
    repo: &RepoRef,
    by_manifest: &BTreeMap<String, Vec<&Candidate>>,
    branch: &str,
    auto_merge: bool,
) -> Result<Option<OpenOutcome>> {
    let default_branch = forge
        .default_branch(repo)
        .await
        .with_context(|| format!("default branch for {}", repo_key(repo)))?;

    // Idempotent: an existing branch is returned, not an error. Created up front
    // so the reads below see prior commits when this is a refresh.
    forge
        .create_branch(repo, branch, &default_branch)
        .await
        .with_context(|| format!("create branch {branch} on {}", repo_key(repo)))?;

    let mut bumped: Vec<(String, String)> = Vec::new();
    for (manifest_path, cands) in by_manifest {
        let Some(provider) = chain.for_manifest(manifest_path) else {
            tracing::warn!("no provider for manifest {manifest_path}; skipping");
            continue;
        };
        // Read from the wave BRANCH: on a refresh this carries prior bumps, so we
        // extend them. Reading the default branch would silently revert them.
        let Some(blob) = forge
            .read_file(repo, manifest_path, branch)
            .await
            .with_context(|| format!("read {manifest_path} on {}", repo_key(repo)))?
        else {
            tracing::warn!("{manifest_path} absent on {}; skipping", repo_key(repo));
            continue;
        };

        let mut text = blob.content;
        let mut changed_here: Vec<(String, String)> = Vec::new();
        for c in cands {
            let (next, changed) = provider.bump(&text, &c.module, &c.latest);
            if changed {
                text = next;
                changed_here.push((c.module.clone(), c.latest.clone()));
            }
        }
        if changed_here.is_empty() {
            // Already at target on this branch — a refresh with nothing new.
            continue;
        }

        let message = format!("Bump {}", summarize(&changed_here));
        forge
            .commit_file(repo, branch, manifest_path, &text, &blob.blob_sha, &message)
            .await
            .with_context(|| format!("commit {manifest_path} on {}", repo_key(repo)))?;
        bumped.extend(changed_here);
    }

    if bumped.is_empty() {
        // Nothing to say. Do NOT open a change — an empty MR is noise, and on a
        // schedule it would be noise every run.
        return Ok(None);
    }

    let title = format!("Bump {}", summarize(&bumped));
    let body = render_body(&bumped);
    let opened = forge
        .open_change(repo, branch, &default_branch, &title, &body, true)
        .await
        .with_context(|| format!("open change on {}", repo_key(repo)))?;

    if auto_merge {
        // Best-effort: a forge with no pipeline yet returns false rather than
        // erroring, and that must not sink an otherwise-good change.
        match forge.enable_auto_merge(repo, &opened.change).await {
            Ok(true) => {}
            Ok(false) => tracing::info!(
                "auto-merge not armed on {} (no pipeline yet?); the change stays open",
                repo_key(repo)
            ),
            Err(e) => tracing::warn!("enable_auto_merge on {} failed: {e}", repo_key(repo)),
        }
    }

    Ok(Some(OpenOutcome {
        repo: repo.clone(),
        url: opened.change.url.clone(),
        already_existed: opened.already_existed,
        bumped,
    }))
}

/// `"@aion/kernel to 0.2.3"` for one, `"7 dependencies"` for many — a title has
/// to stay short, and GitLab caps a commit-status target_url at 255 chars.
fn summarize(bumped: &[(String, String)]) -> String {
    match bumped {
        [(module, target)] => format!("{module} to {target}"),
        _ => format!("{} dependencies", bumped.len()),
    }
}

fn render_body(bumped: &[(String, String)]) -> String {
    let mut s = String::from("Opened by `wave discover --open`.\n\n");
    for (module, target) in bumped {
        s.push_str(&format!("- `{module}` → `{target}`\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_reads_naturally_at_both_sizes() {
        assert_eq!(
            summarize(&[("@aion/kernel".into(), "0.2.3".into())]),
            "@aion/kernel to 0.2.3"
        );
        assert_eq!(
            summarize(&[
                ("@aion/kernel".into(), "0.2.3".into()),
                ("@aion/logger".into(), "0.2.1".into()),
            ]),
            "2 dependencies"
        );
    }

    #[test]
    fn body_lists_every_bump() {
        let body = render_body(&[
            ("@aion/kernel".into(), "0.2.3".into()),
            ("@aion/logger".into(), "0.2.1".into()),
        ]);
        assert!(body.contains("- `@aion/kernel` → `0.2.3`"));
        assert!(body.contains("- `@aion/logger` → `0.2.1`"));
    }
}
