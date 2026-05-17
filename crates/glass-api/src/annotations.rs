//! Annotation read verbs — `annotations`, `db-dump`.
//!
//! Both verbs are read-only views of the on-disk glass-db state.
//! `annotations` returns user-set rename / comment / colour entries
//! for one artifact; `db-dump` returns the bundle-level record
//! (open tabs, expanded paths, etc) keyed by content hash.

use std::path::Path;

use anyhow::{Context, Result};
use glass_db::{Annotation, AnnotationKey, ArtifactId, BundleId, Database};
use serde::Serialize;

#[derive(Serialize, Debug, Clone)]
pub struct AnnotationsResult {
    pub artifact: String,
    pub total: usize,
    pub annotations: Vec<AnnotationEntry>,
}

#[derive(Serialize, Debug, Clone)]
pub struct AnnotationEntry {
    pub key_kind: &'static str,
    /// Stringified key — hex addr / symbol name / class JNI /
    /// `class->method` depending on `key_kind`.
    pub key: String,
    pub kind: &'static str,
    pub value: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct DbDumpResult {
    pub bundle_id: String,
    pub source_path: String,
    pub record: Option<BundleRecordView>,
}

#[derive(Serialize, Debug, Clone)]
pub struct BundleRecordView {
    pub schema_version: u32,
    pub label: String,
    pub last_opened_unix: u64,
    pub artifact_count: usize,
    pub open_tabs: Vec<String>,
    pub active_tab: Option<usize>,
    /// Expanded tree paths, each a vector of usize indices.
    pub expanded_paths: Vec<Vec<usize>>,
    pub source_path: Option<String>,
}

/// Read all annotations stored for the artifact identified by
/// content-hashing the file at `path`. Returns an empty list when
/// no record exists.
pub fn annotations(path: impl AsRef<Path>) -> Result<AnnotationsResult> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let aid = ArtifactId::from_bytes(&bytes);
    let db = Database::open(false).context("opening glass-db (read-only)")?;
    let mut entries: Vec<AnnotationEntry> = db
        .load_annotations(&aid)?
        .into_iter()
        .map(|(k, v)| to_entry(k, v))
        .collect();
    entries.sort_by(|a, b| a.key_kind.cmp(b.key_kind).then(a.key.cmp(&b.key)));
    let total = entries.len();
    Ok(AnnotationsResult {
        artifact: aid.to_string(),
        total,
        annotations: entries,
    })
}

/// Read the bundle-level record for the bundle identified by
/// content-hashing the file at `path`. Returns `record: None` when
/// the bundle has never been opened.
pub fn db_dump(path: impl AsRef<Path>) -> Result<DbDumpResult> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let bid = BundleId::from_bytes(&bytes);
    let db = Database::open(false).context("opening glass-db (read-only)")?;
    let record = db.load_bundle(&bid)?.map(|rec| BundleRecordView {
        schema_version: rec.schema_version as u32,
        label: rec.label,
        last_opened_unix: rec.last_opened_unix,
        artifact_count: rec.artifacts.len(),
        open_tabs: rec.open_tabs.iter().map(|t| format!("{t:?}")).collect(),
        active_tab: rec.active_tab,
        expanded_paths: rec.expanded_paths,
        source_path: rec.source_path,
    });
    Ok(DbDumpResult {
        bundle_id: bid.to_string(),
        source_path: path.display().to_string(),
        record,
    })
}

fn to_entry(key: AnnotationKey, value: Annotation) -> AnnotationEntry {
    let (key_kind, key_str) = match key {
        AnnotationKey::Address(a) => ("address", format!("0x{a:x}")),
        AnnotationKey::Symbol(s) => ("symbol", s),
        AnnotationKey::Class(c) => ("class", c),
        AnnotationKey::Method(c, m) => ("method", format!("{c}->{m}")),
    };
    let (kind, value_str) = match value {
        Annotation::Rename(s) => ("rename", s),
        Annotation::Comment(s) => ("comment", s),
        Annotation::Colour(c) => ("colour", format!("0x{c:08x}")),
    };
    AnnotationEntry {
        key_kind,
        key: key_str,
        kind,
        value: value_str,
    }
}
