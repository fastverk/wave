//! ASCII rendering for plans + waves (the `propose` / `status` output).

// `repo_slug` renders the forge-side plan (WavePlan.items[].repo is a forge
// RepoRef); `pb_repo_slug` renders the stored wave/trace (pb RepoRef).
use forge::repo_slug;
use wave_core::{pb, pb_repo_slug, Candidate, WavePlan};

fn slug_opt(r: &Option<pb::RepoRef>) -> String {
    r.as_ref().map(pb_repo_slug).unwrap_or_default()
}

fn item_state_name(state: i32) -> &'static str {
    use pb::WaveItemState as S;
    match S::try_from(state).unwrap_or(S::Unspecified) {
        S::Unspecified => "UNKNOWN",
        S::Pending => "PENDING",
        S::MrOpen => "MR_OPEN",
        S::CiRunning => "CI_RUNNING",
        S::CiGreen => "CI_GREEN",
        S::Merging => "MERGING",
        S::Merged => "MERGED",
        S::Published => "PUBLISHED",
        S::Skipped => "SKIPPED",
        S::Failed => "FAILED",
    }
}

fn wave_state_name(state: i32) -> &'static str {
    use pb::WaveState as S;
    match S::try_from(state).unwrap_or(S::Unspecified) {
        S::Unspecified => "UNKNOWN",
        S::Proposed => "PROPOSED",
        S::Open => "OPEN",
        S::Merging => "MERGING",
        S::Completed => "COMPLETED",
        S::Aborted => "ABORTED",
    }
}

fn is_done(state: i32) -> bool {
    use pb::WaveItemState as S;
    matches!(
        S::try_from(state).unwrap_or(S::Unspecified),
        S::Published | S::Skipped
    )
}

/// Render a dry-run plan: the tiered cascade + any cycle.
#[must_use]
pub fn render_plan(plan: &WavePlan) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "wave plan: {} → {}\n",
        plan.root_module, plan.target_version
    ));
    if let Some(rr) = &plan.root_repo {
        s.push_str(&format!("  root: {}\n", repo_slug(rr)));
    }
    if plan.items.is_empty() {
        s.push_str("  (no affected consumers)\n");
    }
    for it in &plan.items {
        let decision = it
            .decision
            .as_ref()
            .map(|d| format!(" [{d:?}]"))
            .unwrap_or_default();
        s.push_str(&format!(
            "  tier {} {:<30}{}  deps: {}\n",
            it.tier,
            repo_slug(&it.repo),
            decision,
            it.upstream_modules.join(", ")
        ));
    }
    if !plan.cycle.is_empty() {
        s.push_str(&format!("  ⚠ cycle (unordered): {}\n", plan.cycle.join(", ")));
    }
    s
}

/// Render one wave's per-item cascade state.
#[must_use]
pub fn render_wave(wave: &pb::Wave) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "wave {} [{}]  {} → {}\n",
        wave.id,
        wave_state_name(wave.state),
        wave.root_module,
        wave.target_version
    ));
    for it in &wave.items {
        let change = it
            .change
            .as_ref()
            .filter(|c| !c.url.is_empty())
            .map(|c| format!("  {}", c.url))
            .unwrap_or_default();
        let published = if it.published_version.is_empty() {
            String::new()
        } else {
            format!("  published={}", it.published_version)
        };
        let err = if it.error.is_empty() {
            String::new()
        } else {
            format!("  err: {}", it.error)
        };
        s.push_str(&format!(
            "  tier {} {:<30} {:<11}{change}{published}{err}\n",
            it.tier,
            slug_opt(&it.repo),
            item_state_name(it.state)
        ));
    }
    if !wave.cycle.is_empty() {
        s.push_str(&format!("  ⚠ cycle: {}\n", wave.cycle.join(", ")));
    }
    s
}

/// Human-readable milliseconds: `840ms`, `3.2s`. No float (avoids precision
/// lints) — tenths from the integer remainder.
fn fmt_ms(ms: i64) -> String {
    if ms >= 1000 {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    } else {
        format!("{ms}ms")
    }
}

/// Render a wave trace as a tier-indented waterfall.
#[must_use]
pub fn render_trace(trace: &pb::WaveTrace) -> String {
    let total = if trace.duration_ms > 0 {
        fmt_ms(trace.duration_ms)
    } else {
        "in flight".to_string()
    };
    let mut s = String::new();
    s.push_str(&format!(
        "trace {} [{}]  {} → {}  ({total})\n",
        trace.wave_id,
        wave_state_name(trace.state),
        trace.root_module,
        trace.target_version
    ));
    for span in &trace.spans {
        let indent = "  ".repeat(usize::try_from(span.tier).unwrap_or(0).max(1));
        let dur = if span.duration_ms > 0 {
            fmt_ms(span.duration_ms)
        } else {
            "…".to_string()
        };
        let published = if span.published_version.is_empty() {
            String::new()
        } else {
            format!("  v{}", span.published_version)
        };
        let url = if span.change_url.is_empty() {
            String::new()
        } else {
            format!("  {}", span.change_url)
        };
        s.push_str(&format!(
            "{indent}{:<26} {:<11} {:>7}{published}{url}\n",
            slug_opt(&span.repo),
            item_state_name(span.final_state),
            dur
        ));
    }
    s
}

/// Render the discovery report: external out-of-date deps grouped by repo.
#[must_use]
pub fn render_discovery(candidates: &[Candidate]) -> String {
    if candidates.is_empty() {
        return "no external dependency updates found\n".to_string();
    }
    let repos = candidates
        .iter()
        .map(|c| repo_slug(&c.repo))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let mut s = format!(
        "{} external update(s) across {repos} repo(s):\n",
        candidates.len()
    );
    let mut last = String::new();
    for c in candidates {
        let slug = repo_slug(&c.repo);
        if slug != last {
            s.push_str(&format!("\n{slug}  ({})\n", c.manifest_path));
            slug.clone_into(&mut last);
        }
        s.push_str(&format!("  {:<34} {} → {}\n", c.module, c.current, c.latest));
    }
    s
}

/// Render a one-line-per-wave summary list.
#[must_use]
pub fn render_wave_list(waves: &[pb::Wave]) -> String {
    if waves.is_empty() {
        return "no waves\n".to_string();
    }
    let mut s = String::new();
    for w in waves {
        let done = w.items.iter().filter(|i| is_done(i.state)).count();
        s.push_str(&format!(
            "{:<42} [{:<9}] {} → {}  ({done}/{} done)\n",
            w.id,
            wave_state_name(w.state),
            w.root_module,
            w.target_version,
            w.items.len()
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_wave_states_and_counts() {
        let wave = pb::Wave {
            id: "wave-x".into(),
            root_module: "@s/modules".into(),
            target_version: "0.1.1".into(),
            state: pb::WaveState::Merging as i32,
            items: vec![
                pb::WaveItem {
                    repo: Some(pb::RepoRef {
                        owner: "studio".into(),
                        name: "foundation".into(),
                        ..Default::default()
                    }),
                    tier: 1,
                    state: pb::WaveItemState::Published as i32,
                    published_version: "0.1.1".into(),
                    ..Default::default()
                },
                pb::WaveItem {
                    repo: Some(pb::RepoRef {
                        owner: "studio".into(),
                        name: "web".into(),
                        ..Default::default()
                    }),
                    tier: 2,
                    state: pb::WaveItemState::CiRunning as i32,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let out = render_wave(&wave);
        assert!(out.contains("wave-x [MERGING]"));
        assert!(out.contains("studio/foundation"));
        assert!(out.contains("PUBLISHED"));
        assert!(out.contains("published=0.1.1"));
        assert!(out.contains("CI_RUNNING"));

        let list = render_wave_list(std::slice::from_ref(&wave));
        assert!(list.contains("wave-x"));
        assert!(list.contains("(1/2 done)"));
    }

    #[test]
    fn renders_trace_waterfall_with_tier_indentation() {
        let trace = pb::WaveTrace {
            wave_id: "wave-x".into(),
            root_module: "@s/modules".into(),
            target_version: "0.1.1".into(),
            state: pb::WaveState::Completed as i32,
            duration_ms: 9000,
            spans: vec![
                pb::BumpSpan {
                    repo: Some(pb::RepoRef {
                        owner: "studio".into(),
                        name: "foundation".into(),
                        ..Default::default()
                    }),
                    tier: 1,
                    final_state: pb::WaveItemState::Published as i32,
                    duration_ms: 3000,
                    published_version: "0.1.1".into(),
                    ..Default::default()
                },
                pb::BumpSpan {
                    repo: Some(pb::RepoRef {
                        owner: "studio".into(),
                        name: "web".into(),
                        ..Default::default()
                    }),
                    tier: 2,
                    final_state: pb::WaveItemState::Published as i32,
                    duration_ms: 4000,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let out = render_trace(&trace);
        assert!(out.contains("trace wave-x [COMPLETED]"));
        assert!(out.contains("(9.0s)"));
        // tier-1 indented 2 spaces, tier-2 indented 4 — the cascade depth shows.
        assert!(out.contains("\n  studio/foundation"));
        assert!(out.contains("\n    studio/web"));
        assert!(out.contains("3.0s"));
        assert!(out.contains("v0.1.1"));
    }
}
