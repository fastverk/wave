//! Wave tracing (v1): a durable event log + projection to a [`pb::WaveTrace`].
//!
//! The [`WaveObserver`](crate::runner::WaveObserver) seam emits a
//! [`pb::WaveEvent`] per transition. [`LoggingObserver`] appends each one to a
//! per-wave [`EventLog`] — length-delimited `WaveEvent` protos (the source of
//! truth; DTOs are protos). [`project`] folds the log + the wave's items back
//! into a [`pb::WaveTrace`] of one [`pb::BumpSpan`] per repo (start = first
//! event, end = terminal event), which the CLI renders as a waterfall.
//!
//! Projection is pure + idempotent: re-running it over the same log yields the
//! same trace, so a resuming daemon can re-export at any time.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prost::Message;

use crate::pb;
use crate::runner::WaveObserver;

/// An append-only, per-wave log of length-delimited `WaveEvent` protos.
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    /// The log for `wave_id` under `dir`.
    #[must_use]
    pub fn for_wave(dir: &Path, wave_id: &str) -> Self {
        Self {
            path: dir.join(format!("{wave_id}.log")),
        }
    }

    /// `$FASTVERK_STATE_DIR/wave-events`, else `~/.fastverk/wave-events`.
    #[must_use]
    pub fn default_dir() -> PathBuf {
        let base = std::env::var_os("FASTVERK_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".fastverk")
            });
        base.join("wave-events")
    }

    /// Append one event (length-delimited). Best-effort durability: flushed on
    /// drop of the file handle.
    pub fn append(&self, event: &pb::WaveEvent) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        let buf = event.encode_length_delimited_to_vec();
        f.write_all(&buf)
            .with_context(|| format!("append {}", self.path.display()))?;
        Ok(())
    }

    /// Read every event in order. Absent log = no events.
    pub fn read(&self) -> Result<Vec<pb::WaveEvent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.path).with_context(|| format!("read {}", self.path.display()))?;
        let mut buf = bytes.as_slice();
        let mut out = Vec::new();
        while !buf.is_empty() {
            let ev = pb::WaveEvent::decode_length_delimited(&mut buf)
                .with_context(|| format!("decode event in {}", self.path.display()))?;
            out.push(ev);
        }
        Ok(out)
    }
}

/// A [`WaveObserver`] that appends every event to an [`EventLog`]. Append
/// failures are logged, not propagated — tracing must never break the cascade.
pub struct LoggingObserver {
    log: EventLog,
}

impl LoggingObserver {
    #[must_use]
    pub fn new(log: EventLog) -> Self {
        Self { log }
    }
}

impl WaveObserver for LoggingObserver {
    fn on_event(&self, event: &pb::WaveEvent) {
        if let Err(e) = self.log.append(event) {
            tracing::warn!(error = %e, "wave: failed to append trace event");
        }
    }
}

/// Milliseconds between two RFC 3339 timestamps (`0` if either is empty/unparsable
/// or `end < start`).
#[must_use]
pub fn duration_ms(start: &str, end: &str) -> i64 {
    use chrono::DateTime;
    let (Ok(s), Ok(e)) = (
        DateTime::parse_from_rfc3339(start),
        DateTime::parse_from_rfc3339(end),
    ) else {
        return 0;
    };
    (e - s).num_milliseconds().max(0)
}

fn is_terminal(state: i32) -> bool {
    use pb::WaveItemState as S;
    matches!(
        S::try_from(state).unwrap_or(S::Unspecified),
        S::Published | S::Skipped | S::Failed
    )
}

/// Project a wave + its event log into a [`pb::WaveTrace`] — one span per item,
/// ordered by tier then start. Pure + idempotent.
#[must_use]
pub fn project(wave: &pb::Wave, events: &[pb::WaveEvent]) -> pb::WaveTrace {
    let mut spans: Vec<pb::BumpSpan> = wave
        .items
        .iter()
        .map(|item| {
            let key = item.repo.as_ref().map(crate::pb_repo_key).unwrap_or_default();
            let mine: Vec<&pb::WaveEvent> = events.iter().filter(|e| e.item_key == key).collect();
            let started_at = mine.first().map(|e| e.at.clone()).unwrap_or_default();
            let ended_at = mine
                .iter()
                .rev()
                .find(|e| is_terminal(e.to_state))
                .map(|e| e.at.clone())
                .unwrap_or_default();
            pb::BumpSpan {
                item_key: key,
                repo: item.repo.clone(),
                tier: item.tier,
                final_state: item.state,
                duration_ms: duration_ms(&started_at, &ended_at),
                started_at,
                ended_at,
                change_url: item.change.as_ref().map(|c| c.url.clone()).unwrap_or_default(),
                published_version: item.published_version.clone(),
            }
        })
        .collect();
    spans.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| a.started_at.cmp(&b.started_at)));

    let started_at = spans
        .iter()
        .map(|s| &s.started_at)
        .filter(|s| !s.is_empty())
        .min()
        .cloned()
        .unwrap_or_else(|| wave.created_at.clone());
    let ended_at = if wave.completed_at.is_empty() {
        String::new()
    } else {
        wave.completed_at.clone()
    };

    pb::WaveTrace {
        wave_id: wave.id.clone(),
        root_module: wave.root_module.clone(),
        target_version: wave.target_version.clone(),
        state: wave.state,
        duration_ms: duration_ms(&started_at, &ended_at),
        started_at,
        ended_at,
        spans,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn repo(name: &str) -> pb::RepoRef {
        pb::RepoRef {
            owner: "studio".into(),
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn event_log_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::for_wave(dir.path(), "wave-1");
        assert!(log.read().unwrap().is_empty());
        for (i, to) in [pb::WaveItemState::MrOpen, pb::WaveItemState::Merged]
            .iter()
            .enumerate()
        {
            log.append(&pb::WaveEvent {
                wave_id: "wave-1".into(),
                item_key: "k".into(),
                to_state: *to as i32,
                at: format!("2026-06-18T00:00:0{i}Z"),
                ..Default::default()
            })
            .unwrap();
        }
        let events = log.read().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].to_state, pb::WaveItemState::Merged as i32);
    }

    #[test]
    fn projects_spans_with_durations_in_tier_order() {
        let wave = pb::Wave {
            id: "wave-1".into(),
            root_module: "@s/modules".into(),
            target_version: "0.1.1".into(),
            state: pb::WaveState::Completed as i32,
            completed_at: "2026-06-18T00:00:10Z".into(),
            created_at: "2026-06-18T00:00:00Z".into(),
            items: vec![
                pb::WaveItem {
                    repo: Some(repo("web")),
                    tier: 2,
                    state: pb::WaveItemState::Published as i32,
                    ..Default::default()
                },
                pb::WaveItem {
                    repo: Some(repo("foundation")),
                    tier: 1,
                    state: pb::WaveItemState::Published as i32,
                    published_version: "0.1.1".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let key = |n: &str| crate::pb_repo_key(&repo(n));
        let events = vec![
            pb::WaveEvent {
                item_key: key("foundation"),
                to_state: pb::WaveItemState::MrOpen as i32,
                at: "2026-06-18T00:00:01Z".into(),
                ..Default::default()
            },
            pb::WaveEvent {
                item_key: key("foundation"),
                to_state: pb::WaveItemState::Published as i32,
                at: "2026-06-18T00:00:04Z".into(),
                ..Default::default()
            },
            pb::WaveEvent {
                item_key: key("web"),
                to_state: pb::WaveItemState::MrOpen as i32,
                at: "2026-06-18T00:00:05Z".into(),
                ..Default::default()
            },
            pb::WaveEvent {
                item_key: key("web"),
                to_state: pb::WaveItemState::Published as i32,
                at: "2026-06-18T00:00:09Z".into(),
                ..Default::default()
            },
        ];
        let trace = project(&wave, &events);
        // tier order: foundation (1) before web (2).
        assert_eq!(trace.spans[0].repo.as_ref().unwrap().name, "foundation");
        assert_eq!(trace.spans[1].repo.as_ref().unwrap().name, "web");
        // foundation span: 00:01 → 00:04 = 3000ms.
        assert_eq!(trace.spans[0].duration_ms, 3000);
        // web span: 00:05 → 00:09 = 4000ms.
        assert_eq!(trace.spans[1].duration_ms, 4000);
        // whole wave: first start 00:01 → completed 00:10 = 9000ms.
        assert_eq!(trace.duration_ms, 9000);
    }
}
