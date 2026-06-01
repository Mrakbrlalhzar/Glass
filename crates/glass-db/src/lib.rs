//! glass-db: persistent state for Glass.
//!
//! Stores per-bundle and per-artifact UI state + user annotations in a
//! single redb file on disk. Keyed by blake3 content hashes so that
//! reopening the same binary — from anywhere, on any machine — restores
//! the same view.
//!
//! Lifecycle:
//!   - `Database::open(fresh: bool)` returns a handle. `fresh = true`
//!     gives you an in-memory facade that no-ops reads but still writes
//!     to disk, useful for the `--fresh` CLI flag and for tests.
//!   - Reads happen synchronously when a bundle is loaded — small.
//!   - Writes are debounced: callers mark dirty via `mark_dirty()`, and
//!     a flush task picks up changes every 500ms.
//!
//! The crate is UI-agnostic: nothing here pulls in gpui or any glass-ui
//! types. The `Tab`/`TabState` boundary lives in `glass-ui`.

pub mod ids;
pub mod schema;
mod store;

pub use ids::{ArtifactId, BundleId};
pub use schema::{
    Annotation, AnnotationKey, ArtifactRecord, BookmarkRecord, BundleRecord, ScriptMeta,
    SymbolFilter, TabState,
};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;

/// Top-level handle. Cheap to clone.
#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}

struct Inner {
    store: Mutex<Option<store::Store>>,
    /// When `fresh`, reads return None even if data exists; writes still
    /// land so the new state takes over once the user exits and reopens
    /// without `--fresh`.
    fresh: bool,
    /// Pending writes keyed by hash, drained by `flush()`.
    dirty: Mutex<DirtySet>,
}

#[derive(Default)]
struct DirtySet {
    bundles: std::collections::HashMap<BundleId, BundleRecord>,
    artifacts: std::collections::HashMap<ArtifactId, ArtifactRecord>,
    annotations: std::collections::HashMap<(ArtifactId, AnnotationKey), Option<Annotation>>,
    last_flush: Option<Instant>,
}

impl Database {
    /// Open the database at the platform-standard location. If `fresh`,
    /// reads return None until something is written this session.
    pub fn open(fresh: bool) -> Result<Self> {
        let path = default_db_path()?;
        Self::open_at(&path, fresh)
    }

    /// Open at an explicit path. Used by tests; production code should
    /// prefer `open`.
    pub fn open_at(path: &std::path::Path, fresh: bool) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let store = store::Store::open(path).with_context(|| {
            format!("opening glass-db at {}", path.display())
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                store: Mutex::new(Some(store)),
                fresh,
                dirty: Mutex::new(DirtySet::default()),
            }),
        })
    }

    pub fn is_fresh(&self) -> bool {
        self.inner.fresh
    }

    // ---- reads --------------------------------------------------------------

    /// Up to `limit` most-recently-opened bundles. Skips entries
    /// without a `source_path` (older records that pre-date the
    /// field). Sorted newest first. Deduplicated by `source_path` —
    /// rebuilding an APK produces a fresh content-hash bundle id
    /// each time, but the path on disk is what the user identifies
    /// the file by, so we collapse to the newest record per path.
    pub fn recent_bundles(&self, limit: usize) -> Vec<BundleRecord> {
        if self.inner.fresh {
            return Vec::new();
        }
        let store = self.inner.store.lock();
        let Some(s) = store.as_ref() else { return Vec::new() };
        let Ok(mut all) = s.read_all_bundles() else { return Vec::new() };
        all.retain(|b| b.source_path.is_some());
        all.sort_by(|a, b| b.last_opened_unix.cmp(&a.last_opened_unix));
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        all.retain(|b| match b.source_path.as_deref() {
            Some(p) => seen.insert(p.to_string()),
            None => false,
        });
        all.truncate(limit);
        all
    }

    pub fn load_bundle(&self, id: &BundleId) -> Result<Option<BundleRecord>> {
        if self.inner.fresh {
            return Ok(None);
        }
        let store = self.inner.store.lock();
        match store.as_ref() {
            Some(s) => s.read_bundle(id),
            None => Ok(None),
        }
    }

    pub fn load_artifact(&self, id: &ArtifactId) -> Result<Option<ArtifactRecord>> {
        if self.inner.fresh {
            return Ok(None);
        }
        let store = self.inner.store.lock();
        match store.as_ref() {
            Some(s) => s.read_artifact(id),
            None => Ok(None),
        }
    }

    pub fn load_annotations(
        &self,
        id: &ArtifactId,
    ) -> Result<Vec<(AnnotationKey, Annotation)>> {
        if self.inner.fresh {
            return Ok(Vec::new());
        }
        let store = self.inner.store.lock();
        match store.as_ref() {
            Some(s) => s.read_annotations(id),
            None => Ok(Vec::new()),
        }
    }

    // ---- staged writes ------------------------------------------------------
    //
    // These don't touch disk. They batch into `DirtySet`; a periodic
    // `flush()` call (driven by the UI's existing timer task) writes them.

    pub fn save_bundle(&self, id: BundleId, rec: BundleRecord) {
        self.inner.dirty.lock().bundles.insert(id, rec);
    }

    pub fn save_artifact(&self, id: ArtifactId, rec: ArtifactRecord) {
        self.inner.dirty.lock().artifacts.insert(id, rec);
    }

    pub fn set_annotation(
        &self,
        artifact: ArtifactId,
        key: AnnotationKey,
        value: Annotation,
    ) {
        self.inner
            .dirty
            .lock()
            .annotations
            .insert((artifact, key), Some(value));
    }

    pub fn clear_annotation(&self, artifact: ArtifactId, key: AnnotationKey) {
        self.inner
            .dirty
            .lock()
            .annotations
            .insert((artifact, key), None);
    }

    // ---- Frida script metadata ---------------------------------------------
    //
    // Scripts are infrequent UI events (write, delete, toggle), so we
    // commit synchronously rather than going through the debounced
    // dirty set. Keeps the call sites simple — no "did my enable get
    // flushed yet" race when the UI shells out to the actor.

    /// All script metadata, keyed by name. Returns an empty map when
    /// no scripts have been registered yet (or in fresh mode).
    pub fn all_script_meta(&self) -> std::collections::HashMap<String, schema::ScriptMeta> {
        if self.inner.fresh {
            return std::collections::HashMap::new();
        }
        let store = self.inner.store.lock();
        let Some(s) = store.as_ref() else {
            return std::collections::HashMap::new();
        };
        s.read_all_script_meta().unwrap_or_default()
    }

    pub fn script_meta(&self, name: &str) -> Option<schema::ScriptMeta> {
        if self.inner.fresh {
            return None;
        }
        let store = self.inner.store.lock();
        let s = store.as_ref()?;
        s.read_script_meta(name).ok().flatten()
    }

    pub fn save_script_meta(
        &self,
        name: &str,
        meta: &schema::ScriptMeta,
    ) -> Result<()> {
        let store = self.inner.store.lock();
        let Some(s) = store.as_ref() else { return Ok(()) };
        s.write_script_meta(name, meta)
    }

    /// Remove the script's metadata + every per-bundle enabled row
    /// that mentions it. Caller is responsible for deleting the
    /// on-disk file.
    pub fn delete_script(&self, name: &str) -> Result<()> {
        let store = self.inner.store.lock();
        let Some(s) = store.as_ref() else { return Ok(()) };
        s.delete_script(name)
    }

    /// Names enabled for the given bundle, sorted.
    pub fn enabled_scripts(&self, bundle: &BundleId) -> Vec<String> {
        if self.inner.fresh {
            return Vec::new();
        }
        let store = self.inner.store.lock();
        let Some(s) = store.as_ref() else { return Vec::new() };
        s.read_enabled_scripts(bundle).unwrap_or_default()
    }

    pub fn set_script_enabled(
        &self,
        bundle: &BundleId,
        name: &str,
        enabled: bool,
    ) -> Result<()> {
        let store = self.inner.store.lock();
        let Some(s) = store.as_ref() else { return Ok(()) };
        s.set_script_enabled(bundle, name, enabled)
    }

    /// Flush all pending writes. Cheap when nothing is dirty.
    /// Callers should call this no more than once every ~500ms.
    pub fn flush(&self) -> Result<()> {
        let snapshot = {
            let mut dirty = self.inner.dirty.lock();
            if dirty.bundles.is_empty()
                && dirty.artifacts.is_empty()
                && dirty.annotations.is_empty()
            {
                return Ok(());
            }
            let taken = DirtySet {
                bundles: std::mem::take(&mut dirty.bundles),
                artifacts: std::mem::take(&mut dirty.artifacts),
                annotations: std::mem::take(&mut dirty.annotations),
                last_flush: None,
            };
            dirty.last_flush = Some(Instant::now());
            taken
        };

        let store_guard = self.inner.store.lock();
        let store = match store_guard.as_ref() {
            Some(s) => s,
            None => return Ok(()),
        };
        store.write_batch(&snapshot.bundles, &snapshot.artifacts, &snapshot.annotations)
    }

    /// Suggested flush interval — UI callers can use this to size their
    /// debounce timer. Kept on the struct so we can tune in one place.
    pub fn flush_interval(&self) -> Duration {
        Duration::from_millis(500)
    }
}

/// Filesystem path the persistence layer uses by default. Exposed
/// so external watchers (e.g. the GUI's annotation-reload poll)
/// can monitor mtime / fs events on the file without duplicating
/// the resolution logic.
pub fn default_db_path() -> Result<PathBuf> {
    let base = dirs::data_dir().context("no platform data dir (HOME unset?)")?;
    Ok(base.join("Glass").join("glass.redb"))
}

/// Filesystem directory holding the user's Frida script library.
/// One `.js` file per script. Created lazily — call this and then
/// `std::fs::create_dir_all` on the result; we don't auto-create
/// here because the read-only paths in glass-api don't want the
/// side effect.
pub fn scripts_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().context("no platform data dir (HOME unset?)")?;
    Ok(base.join("Glass").join("scripts"))
}

// ---- window-size persistence -----------------------------------------------
//
// Stored alongside the redb file as plain JSON so it's easy to inspect
// and edit by hand, and doesn't clash with the bundle-keyed state.

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct WindowSettings {
    pub bounds: Option<StoredBounds>,
    /// Active theme, by `name` (matches the `name` field in a theme
    /// JSON or a built-in). `None` → built-in default. `#[serde(default)]`
    /// so older settings.json files load unchanged.
    #[serde(default)]
    pub theme: Option<String>,
    /// Override path for the `adb` binary. When `None`, glass-
    /// device falls back to its default discovery order
    /// ($PATH, $ANDROID_HOME, common Android Studio + Homebrew
    /// locations). Set this when adb is installed somewhere
    /// unusual and Glass can't find it on its own.
    #[serde(default)]
    pub adb_path: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy)]
pub struct StoredBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

fn settings_path() -> Result<PathBuf> {
    let base = dirs::data_dir().context("no platform data dir")?;
    Ok(base.join("Glass").join("settings.json"))
}

/// Directory where user-installed theme JSONs live. Built-ins are
/// compiled into the binary and merged with whatever's on disk.
/// Returns the path even if the directory doesn't exist yet — callers
/// that read from it should tolerate a missing dir.
pub fn themes_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().context("no platform data dir")?;
    Ok(base.join("Glass").join("Themes"))
}

pub fn load_window_settings() -> WindowSettings {
    let Ok(path) = settings_path() else { return WindowSettings::default() };
    let Ok(bytes) = std::fs::read(&path) else { return WindowSettings::default() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_window_settings(settings: &WindowSettings) -> Result<()> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(settings)?;
    std::fs::write(&path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use schema::*;

    #[test]
    fn round_trip_bundle() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.redb");

        let db = Database::open_at(&path, false)?;
        let bid = BundleId::from_bytes(&[1, 2, 3]);
        let rec = BundleRecord {
            schema_version: schema::SCHEMA_VERSION,
            label: "teapot.apk".into(),
            last_opened_unix: 1,
            artifacts: vec![ArtifactId::from_bytes(&[9, 9, 9])],
            open_tabs: vec![TabState::SmaliClass {
                class_jni: "Lcom/example/Foo;".into(),
                scroll_line: 0,
            }],
            active_tab: Some(0),
            expanded_paths: vec![],
            source_path: None,
            annotations_pane_open: false,
            window_tint: 0,
        };
        db.save_bundle(bid.clone(), rec.clone());
        db.flush()?;
        drop(db);

        let db = Database::open_at(&path, false)?;
        let got = db.load_bundle(&bid)?.expect("bundle persisted");
        assert_eq!(got.label, "teapot.apk");
        assert_eq!(got.open_tabs.len(), 1);
        Ok(())
    }

    #[test]
    fn fresh_mode_skips_reads_but_persists_writes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.redb");

        // Write something via a normal handle.
        let db = Database::open_at(&path, false)?;
        let bid = BundleId::from_bytes(&[7]);
        db.save_bundle(
            bid.clone(),
            BundleRecord {
                schema_version: schema::SCHEMA_VERSION,
                label: "x".into(),
                last_opened_unix: 0,
                artifacts: vec![],
                open_tabs: vec![],
                active_tab: None,
                expanded_paths: vec![],
                source_path: None,
            annotations_pane_open: false,
            window_tint: 0,
            },
        );
        db.flush()?;
        drop(db);

        // Fresh handle: read returns None, but a subsequent write replaces.
        let fresh = Database::open_at(&path, true)?;
        assert!(fresh.load_bundle(&bid)?.is_none());
        fresh.save_bundle(
            bid.clone(),
            BundleRecord {
                schema_version: schema::SCHEMA_VERSION,
                label: "y".into(),
                last_opened_unix: 0,
                artifacts: vec![],
                open_tabs: vec![],
                active_tab: None,
                expanded_paths: vec![],
                source_path: None,
            annotations_pane_open: false,
            window_tint: 0,
            },
        );
        fresh.flush()?;
        drop(fresh);

        // Re-open normally: y wins.
        let db = Database::open_at(&path, false)?;
        assert_eq!(db.load_bundle(&bid)?.unwrap().label, "y");
        Ok(())
    }

    #[test]
    fn annotations_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Database::open_at(&dir.path().join("a.redb"), false)?;
        let aid = ArtifactId::from_bytes(&[0xab]);
        db.set_annotation(
            aid.clone(),
            AnnotationKey::Class("Lcom/example/Foo;".into()),
            Annotation {
                comment: Some("the interesting one".into()),
                ..Default::default()
            },
        );
        db.set_annotation(
            aid.clone(),
            AnnotationKey::Address(0x1234),
            Annotation {
                colour: Some(0xff0000ff),
                ..Default::default()
            },
        );
        db.flush()?;

        let mut got = db.load_annotations(&aid)?;
        got.sort_by(|a, b| format!("{:?}", a.0).cmp(&format!("{:?}", b.0)));
        assert_eq!(got.len(), 2);

        db.clear_annotation(aid.clone(), AnnotationKey::Address(0x1234));
        db.flush()?;
        assert_eq!(db.load_annotations(&aid)?.len(), 1);
        Ok(())
    }
}
