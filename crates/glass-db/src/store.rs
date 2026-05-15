//! Thin wrapper over `redb` that knows our four tables and serialises
//! records as JSON. JSON is the right choice for v1 because:
//!   - records are small (one per bundle/artifact),
//!   - schema evolves quickly, and self-describing wire format helps,
//!   - a corrupt DB can be inspected with `jq` while debugging.
//! We can swap to bincode behind this module later if profiling shows
//! it matters.

use anyhow::{Context, Result};
use redb::{Database as RedbDb, TableDefinition};
use std::collections::HashMap;
use std::path::Path;

use crate::ids::{ArtifactId, BundleId};
use crate::schema::{Annotation, AnnotationKey, ArtifactRecord, BundleRecord, SCHEMA_VERSION};

// Keys are the 32-byte hashes; values are JSON blobs. Annotations use a
// composite key (artifact || serialized AnnotationKey) so we can scan
// all annotations for a given artifact via range queries.
const BUNDLES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("bundles");
const ARTIFACTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("artifacts");
const ANNOTATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("annotations");

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
