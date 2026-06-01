//! Thin wrapper over `redb` that knows our four tables and serialises
//! records as JSON. JSON is the right choice for v1 because:
//!
//! - records are small (one per bundle/artifact),
//! - schema evolves quickly, and self-describing wire format helps,
//! - a corrupt DB can be inspected with `jq` while debugging.
//!
//! We can swap to bincode behind this module later if profiling shows
//! it matters.

use anyhow::{Context, Result};
use redb::{Database as RedbDb, TableDefinition};
use std::collections::HashMap;
use std::path::Path;

use crate::ids::{ArtifactId, BundleId};
use crate::schema::{
    Annotation, AnnotationKey, ArtifactRecord, BundleRecord, ScriptMeta, SCHEMA_VERSION,
};

// Keys are the 32-byte hashes; values are JSON blobs. Annotations use a
// composite key (artifact || serialized AnnotationKey) so we can scan
// all annotations for a given artifact via range queries.
const BUNDLES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("bundles");
const ARTIFACTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("artifacts");
const ANNOTATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("annotations");
// Frida script metadata, keyed by script name (no `.js`).
// Global — not bound to any bundle.
const SCRIPT_META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("script_meta");
// Per-bundle enabled flag. Composite key = bundle_id (32 bytes) ||
// script name. Value is the byte `1`. Absence ⇒ disabled.
const SCRIPT_ENABLED: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("script_enabled");

pub(crate) struct Store {
    db: RedbDb,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = RedbDb::create(path)
            .with_context(|| format!("redb create at {}", path.display()))?;
        // Touch each table once so they exist for read-only sessions.
        let tx = db.begin_write()?;
        {
            let _ = tx.open_table(BUNDLES)?;
            let _ = tx.open_table(ARTIFACTS)?;
            let _ = tx.open_table(ANNOTATIONS)?;
            let _ = tx.open_table(SCRIPT_META)?;
            let _ = tx.open_table(SCRIPT_ENABLED)?;
        }
        tx.commit()?;
        Ok(Self { db })
    }

    // ---- reads --------------------------------------------------------------

    pub fn read_bundle(&self, id: &BundleId) -> Result<Option<BundleRecord>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BUNDLES)?;
        let Some(blob) = table.get(id.as_bytes().as_slice())? else {
            return Ok(None);
        };
        Ok(decode_versioned(blob.value()))
    }

    /// Yield every stored bundle record. Used to power Open Recent
    /// menus and similar history surfaces. Unparseable rows are
    /// silently skipped — easier than failing the whole call.
    pub fn read_all_bundles(&self) -> Result<Vec<BundleRecord>> {
        use redb::ReadableTable;
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BUNDLES)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_k, v) = entry?;
            if let Some(rec) = decode_versioned::<BundleRecord>(v.value()) {
                out.push(rec);
            }
        }
        Ok(out)
    }

    pub fn read_artifact(&self, id: &ArtifactId) -> Result<Option<ArtifactRecord>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(ARTIFACTS)?;
        let Some(blob) = table.get(id.as_bytes().as_slice())? else {
            return Ok(None);
        };
        Ok(decode_versioned(blob.value()))
    }

    pub fn read_annotations(
        &self,
        id: &ArtifactId,
    ) -> Result<Vec<(AnnotationKey, Annotation)>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(ANNOTATIONS)?;
        let prefix = id.as_bytes().as_slice();
        let mut start = prefix.to_vec();
        let mut end = prefix.to_vec();
        // Lexicographic next: increment the last byte; if it overflows we
        // scan past the 32-byte prefix entirely, which is still safe.
        for byte in end.iter_mut().rev() {
            if *byte != 0xff {
                *byte += 1;
                break;
            }
            *byte = 0;
        }
        // Append a separator we don't expect to find inside any key so
        // the range strictly excludes other artifacts. Using one trailing
        // 0xff suffices because composite keys always have ≥33 bytes.
        start.push(0);

        let mut out = Vec::new();
        for entry in table.range(start.as_slice()..end.as_slice())? {
            let (k, v) = entry?;
            let key_bytes = k.value();
            if !key_bytes.starts_with(prefix) {
                continue;
            }
            // The key suffix is the JSON-encoded AnnotationKey.
            let Ok(key) = serde_json::from_slice::<AnnotationKey>(&key_bytes[prefix.len()..])
            else {
                continue;
            };
            if let Some(value) = decode_versioned::<Annotation>(v.value()) {
                out.push((key, value));
            }
        }
        Ok(out)
    }

    // ---- Frida scripts ------------------------------------------------------

    /// All script metadata records, keyed by name. Names not on
    /// disk are still returned — callers reconcile against the
    /// script directory and report orphan rows separately.
    pub fn read_all_script_meta(&self) -> Result<HashMap<String, ScriptMeta>> {
        use redb::ReadableTable;
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SCRIPT_META)?;
        let mut out = HashMap::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            let Ok(name) = std::str::from_utf8(k.value()) else { continue };
            if let Some(meta) = decode_versioned::<ScriptMeta>(v.value()) {
                out.insert(name.to_string(), meta);
            }
        }
        Ok(out)
    }

    pub fn read_script_meta(&self, name: &str) -> Result<Option<ScriptMeta>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SCRIPT_META)?;
        let Some(blob) = table.get(name.as_bytes())? else {
            return Ok(None);
        };
        Ok(decode_versioned(blob.value()))
    }

    pub fn write_script_meta(&self, name: &str, meta: &ScriptMeta) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(SCRIPT_META)?;
            let blob = encode(meta)?;
            t.insert(name.as_bytes(), blob.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove a script's metadata + every per-bundle enabled row
    /// referring to it. Idempotent.
    pub fn delete_script(&self, name: &str) -> Result<()> {
        use redb::ReadableTable;
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(SCRIPT_META)?;
            t.remove(name.as_bytes())?;
        }
        {
            let mut t = tx.open_table(SCRIPT_ENABLED)?;
            // Composite key suffix is `name`; with bundle ids being
            // 32 bytes we scan and collect matches first, then
            // remove. The table is small (one row per
            // (bundle, enabled-script) pair) so a full scan is
            // cheap.
            let mut to_remove = Vec::new();
            for entry in t.iter()? {
                let (k, _v) = entry?;
                let key = k.value();
                if key.len() > 32 && &key[32..] == name.as_bytes() {
                    to_remove.push(key.to_vec());
                }
            }
            for k in to_remove {
                t.remove(k.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Names enabled for the given bundle, sorted.
    pub fn read_enabled_scripts(
        &self,
        bundle: &BundleId,
    ) -> Result<Vec<String>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SCRIPT_ENABLED)?;
        let prefix = bundle.as_bytes().as_slice();
        let mut end = prefix.to_vec();
        for byte in end.iter_mut().rev() {
            if *byte != 0xff {
                *byte += 1;
                break;
            }
            *byte = 0;
        }
        let mut out = Vec::new();
        for entry in table.range(prefix..end.as_slice())? {
            let (k, _v) = entry?;
            let key = k.value();
            if key.len() < 32 || !key.starts_with(prefix) {
                continue;
            }
            if let Ok(name) = std::str::from_utf8(&key[32..]) {
                out.push(name.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn set_script_enabled(
        &self,
        bundle: &BundleId,
        name: &str,
        enabled: bool,
    ) -> Result<()> {
        let mut key = bundle.as_bytes().to_vec();
        key.extend_from_slice(name.as_bytes());
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(SCRIPT_ENABLED)?;
            if enabled {
                t.insert(key.as_slice(), &[1u8][..])?;
            } else {
                t.remove(key.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- writes -------------------------------------------------------------

    pub fn write_batch(
        &self,
        bundles: &HashMap<BundleId, BundleRecord>,
        artifacts: &HashMap<ArtifactId, ArtifactRecord>,
        annotations: &HashMap<(ArtifactId, AnnotationKey), Option<Annotation>>,
    ) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(BUNDLES)?;
            for (id, rec) in bundles {
                let blob = encode(rec)?;
                t.insert(id.as_bytes().as_slice(), blob.as_slice())?;
            }
        }
        {
            let mut t = tx.open_table(ARTIFACTS)?;
            for (id, rec) in artifacts {
                let blob = encode(rec)?;
                t.insert(id.as_bytes().as_slice(), blob.as_slice())?;
            }
        }
        {
            let mut t = tx.open_table(ANNOTATIONS)?;
            for ((aid, ak), value) in annotations {
                let mut key = aid.as_bytes().to_vec();
                key.extend(serde_json::to_vec(ak)?);
                match value {
                    Some(v) => {
                        let blob = encode(v)?;
                        t.insert(key.as_slice(), blob.as_slice())?;
                    }
                    None => {
                        t.remove(key.as_slice())?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

// ---- encode helpers ---------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct Versioned<T> {
    v: u32,
    #[serde(rename = "d")]
    data: T,
}

fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let wrapper = Versioned {
        v: SCHEMA_VERSION,
        data: value,
    };
    Ok(serde_json::to_vec(&wrapper)?)
}

fn decode_versioned<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    let parsed: Versioned<T> = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "glass-db: skipping unreadable record");
            return None;
        }
    };
    if parsed.v > SCHEMA_VERSION {
        tracing::warn!(
            record_version = parsed.v,
            supported = SCHEMA_VERSION,
            "glass-db: record newer than this build, skipping"
        );
        return None;
    }
    Some(parsed.data)
}
