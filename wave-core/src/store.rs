//! Durable wave store.
//!
//! Every wave + its latest state, persisted as the `wave.v1.WaveStore` proto
//! (prost binary) at a single file (default `~/.fastverk/wave-store.pb`, override
//! with `FASTVERK_STATE_DIR`). The on-disk shape is the proto itself — DTOs are
//! protos — so the engine, CLI/daemon, and trace exporter all read one schema.
//!
//! Writes are atomic-ish: encode to a tmp file, then rename. Operations run
//! under a `Mutex` — fine for the low-throughput reconcile surface; revisit if a
//! daemon ever drives many waves concurrently.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use prost::Message;

use crate::pb;

const SCHEMA_VERSION: u32 = 1;

/// Persistent store wrapping the `WaveStore` proto on disk.
pub struct Store {
    path: PathBuf,
    inner: Mutex<pb::WaveStore>,
}

impl Store {
    /// Open (or initialize) the store at `path`.
    pub fn open(path: PathBuf) -> Result<Self> {
        let inner = if path.exists() {
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let mut s = pb::WaveStore::decode(bytes.as_slice())
                .with_context(|| format!("decode {}", path.display()))?;
            s.schema_version = SCHEMA_VERSION;
            s
        } else {
            pb::WaveStore {
                schema_version: SCHEMA_VERSION,
                waves: Vec::new(),
            }
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    /// The default store path: `$FASTVERK_STATE_DIR/wave-store.pb`, else
    /// `~/.fastverk/wave-store.pb`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        let dir = std::env::var_os("FASTVERK_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".fastverk")
            });
        dir.join("wave-store.pb")
    }

    /// All waves, newest-state.
    pub fn waves(&self) -> Vec<pb::Wave> {
        self.inner.lock().unwrap().waves.clone()
    }

    /// One wave by id.
    pub fn get(&self, id: &str) -> Option<pb::Wave> {
        self.inner
            .lock()
            .unwrap()
            .waves
            .iter()
            .find(|w| w.id == id)
            .cloned()
    }

    /// Insert or replace a wave by id, then persist.
    pub fn upsert(&self, wave: pb::Wave) -> Result<()> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(slot) = guard.waves.iter_mut().find(|w| w.id == wave.id) {
            *slot = wave;
        } else {
            guard.waves.push(wave);
        }
        guard.schema_version = SCHEMA_VERSION;
        Self::persist(&self.path, &guard)
    }

    fn persist(path: &Path, store: &pb::WaveStore) -> Result<()> {
        let bytes = store.encode_to_vec();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("pb.tmp");
        fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_roundtrips_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wave-store.pb");

        let store = Store::open(path.clone()).unwrap();
        assert!(store.waves().is_empty());

        store
            .upsert(pb::Wave {
                id: "wave-1".into(),
                root_module: "@s/modules".into(),
                target_version: "0.1.1".into(),
                state: pb::WaveState::Open as i32,
                ..Default::default()
            })
            .unwrap();

        // Reopen from disk → the wave persisted.
        let reopened = Store::open(path.clone()).unwrap();
        assert_eq!(reopened.waves().len(), 1);
        assert_eq!(reopened.get("wave-1").unwrap().root_module, "@s/modules");

        // Upsert with the same id replaces (no duplicate row).
        reopened
            .upsert(pb::Wave {
                id: "wave-1".into(),
                root_module: "@s/modules".into(),
                target_version: "0.1.1".into(),
                state: pb::WaveState::Completed as i32,
                ..Default::default()
            })
            .unwrap();
        let again = Store::open(path).unwrap();
        assert_eq!(again.waves().len(), 1);
        assert_eq!(again.get("wave-1").unwrap().state, pb::WaveState::Completed as i32);
    }
}
