//! Persisted panel layout store (owned by the server unit).
//!
//! One JSON document, `<state-dir>/layout.json`, written atomically with mode 0600:
//! `{"version":u64,"updated_at":ms|null,"layout":<any JSON>|null}`. The layout itself is opaque
//! to the server; `version` counts the successful writes and survives restarts.

use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// File name inside the state directory.
pub const LAYOUT_FILE: &str = "layout.json";

/// Largest accepted layout, measured as its compact JSON serialisation.
pub const MAX_LAYOUT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LayoutDoc {
    pub version: u64,
    pub updated_at: Option<i64>,
    pub layout: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("layout is larger than {MAX_LAYOUT_BYTES} bytes")]
    TooLarge,
    #[error("could not save the layout: {0}")]
    Io(#[from] io::Error),
}

pub struct LayoutStore {
    path: PathBuf,
    doc: Mutex<LayoutDoc>,
}

impl LayoutStore {
    /// Loads `<state_dir>/layout.json` when there is one. A missing file starts empty; an
    /// unreadable one is moved aside (`layout.json.corrupt-<ms>`) and the store starts empty,
    /// so a damaged file can never stop the server from starting.
    pub fn open(state_dir: &Path) -> io::Result<LayoutStore> {
        util::ensure_private_dir(state_dir)?;
        let path = state_dir.join(LAYOUT_FILE);
        let doc = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<LayoutDoc>(&bytes) {
                Ok(doc) => doc,
                Err(err) => {
                    let aside = state_dir.join(format!("{LAYOUT_FILE}.corrupt-{}", util::now_ms()));
                    tracing::warn!(path = %path.display(), %err, aside = %aside.display(), "layout file is unreadable; starting with an empty layout");
                    // Best effort: losing the damaged copy is no worse than not having moved it.
                    let _ = fs::rename(&path, &aside);
                    LayoutDoc::default()
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => LayoutDoc::default(),
            Err(err) => return Err(err),
        };
        Ok(LayoutStore {
            path,
            doc: Mutex::new(doc),
        })
    }

    pub fn get(&self) -> LayoutDoc {
        self.lock().clone()
    }

    /// Replaces the layout: `version` goes up by one and `updated_at` becomes now. The
    /// in-memory document only changes when the file was written.
    pub fn put(&self, layout: Value) -> Result<LayoutDoc, LayoutError> {
        let size = serde_json::to_vec(&layout)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?
            .len();
        if size > MAX_LAYOUT_BYTES {
            return Err(LayoutError::TooLarge);
        }
        let mut current = self.lock();
        let next = LayoutDoc {
            version: current.version.saturating_add(1),
            updated_at: Some(util::now_ms()),
            layout: Some(layout),
        };
        let bytes = serde_json::to_vec(&next)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        util::atomic_write(&self.path, &bytes, 0o600)?;
        *current = next.clone();
        Ok(next)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LayoutDoc> {
        self.doc.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_new_store_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayoutStore::open(dir.path()).unwrap();
        assert_eq!(
            serde_json::to_value(store.get()).unwrap(),
            json!({"version": 0, "updated_at": null, "layout": null})
        );
        assert!(!dir.path().join(LAYOUT_FILE).exists());
    }

    #[test]
    fn put_increments_the_version_and_stamps_the_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayoutStore::open(dir.path()).unwrap();
        let before = util::now_ms();
        let first = store
            .put(json!({"schema": 1, "dockview": {"a": 1}}))
            .unwrap();
        let second = store
            .put(json!({"schema": 1, "dockview": {"a": 2}}))
            .unwrap();
        let after = util::now_ms();
        assert_eq!((first.version, second.version), (1, 2));
        for doc in [&first, &second] {
            let at = doc.updated_at.expect("updated_at is set by a put");
            assert!(
                (before..=after).contains(&at),
                "{at} not in {before}..={after}"
            );
        }
        assert!(second.updated_at >= first.updated_at);
        assert_eq!(store.get(), second);
        assert_eq!(
            store.get().layout,
            Some(json!({"schema": 1, "dockview": {"a": 2}}))
        );
    }

    #[test]
    fn any_json_value_is_stored_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayoutStore::open(dir.path()).unwrap();
        for value in [
            json!(null),
            json!("text"),
            json!(7),
            json!([1, [2]]),
            json!({"é": "ü"}),
        ] {
            let doc = store.put(value.clone()).unwrap();
            assert_eq!(doc.layout, Some(value));
        }
    }

    #[test]
    fn the_document_survives_reopening_and_the_version_keeps_counting() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = LayoutStore::open(dir.path()).unwrap();
            store.put(json!({"n": 1})).unwrap();
            store.put(json!({"n": 2})).unwrap();
        }
        let reopened = LayoutStore::open(dir.path()).unwrap();
        let doc = reopened.get();
        assert_eq!(doc.version, 2);
        assert_eq!(doc.layout, Some(json!({"n": 2})));
        assert!(doc.updated_at.is_some());
        assert_eq!(reopened.put(json!({"n": 3})).unwrap().version, 3);
    }

    #[test]
    fn the_file_has_the_documented_shape_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayoutStore::open(dir.path()).unwrap();
        let doc = store.put(json!({"k": "v"})).unwrap();
        let path = dir.path().join(LAYOUT_FILE);
        let on_disk: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk,
            json!({"version": 1, "updated_at": doc.updated_at, "layout": {"k": "v"}})
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn writes_leave_no_temporary_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayoutStore::open(dir.path()).unwrap();
        for n in 0..5 {
            store.put(json!({ "n": n })).unwrap();
        }
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [LAYOUT_FILE]);
    }

    #[test]
    fn a_layout_over_the_limit_is_refused_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayoutStore::open(dir.path()).unwrap();
        let kept = store.put(json!({"keep": true})).unwrap();

        let too_big = json!({"blob": "x".repeat(MAX_LAYOUT_BYTES)});
        assert!(matches!(store.put(too_big), Err(LayoutError::TooLarge)));
        assert_eq!(store.get(), kept, "a refused put must not touch the store");

        // Exactly at the limit is fine: {"b":"…"} is 8 bytes of envelope.
        let envelope = serde_json::to_vec(&json!({"b": ""})).unwrap().len();
        let fits = json!({"b": "x".repeat(MAX_LAYOUT_BYTES - envelope)});
        assert_eq!(serde_json::to_vec(&fits).unwrap().len(), MAX_LAYOUT_BYTES);
        assert_eq!(store.put(fits).unwrap().version, 2);
        let one_more = json!({"b": "x".repeat(MAX_LAYOUT_BYTES - envelope + 1)});
        assert!(matches!(store.put(one_more), Err(LayoutError::TooLarge)));
    }

    #[test]
    fn a_failed_write_leaves_the_in_memory_document_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = LayoutStore::open(&state).unwrap();
        let kept = store.put(json!({"v": 1})).unwrap();
        // Make the destination impossible to replace: a non-empty directory.
        fs::remove_file(state.join(LAYOUT_FILE)).unwrap();
        fs::create_dir(state.join(LAYOUT_FILE)).unwrap();
        fs::write(state.join(LAYOUT_FILE).join("child"), b"x").unwrap();
        assert!(matches!(
            store.put(json!({"v": 2})),
            Err(LayoutError::Io(_))
        ));
        assert_eq!(store.get(), kept);
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_and_the_store_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(LAYOUT_FILE), b"{ not json").unwrap();
        let store = LayoutStore::open(dir.path()).unwrap();
        assert_eq!(store.get(), LayoutDoc::default());
        let aside: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("layout.json.corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1, "the damaged file is kept for inspection");
        assert_eq!(store.put(json!({"fresh": true})).unwrap().version, 1);
    }

    #[test]
    fn concurrent_puts_never_reuse_a_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(LayoutStore::open(dir.path()).unwrap());
        let mut handles = Vec::new();
        for worker in 0..4 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                (0..10)
                    .map(|n| store.put(json!({"w": worker, "n": n})).unwrap().version)
                    .collect::<Vec<_>>()
            }));
        }
        let mut versions: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        versions.sort_unstable();
        assert_eq!(versions, (1..=40).collect::<Vec<u64>>());
    }
}
