//! Annotation read + write verbs.
//!
//! Reads: `annotations` (per-artifact) and `db_dump` (bundle-level
//! record — tabs, expanded paths, etc). Writes: `set_rename`,
//! `set_comment`, `set_colour`, `clear_annotation` — one verb per
//! `Annotation` variant plus a remover.
//!
//! All persistence goes through `glass_db::Database`; every write
//! verb opens the DB, applies the change, calls `flush()`. The
//! DB is content-addressed by artifact hash, so the same .so or
//! .dylib shipped in two different bundles shares annotations.

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
    /// Three facets, each independent — a row can carry any
    /// combination. `colour` is rendered as `0xRRGGBBAA`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
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

// ---- Write verbs ---------------------------------------------------------

/// What was written. Mirrors the shape of `AnnotationEntry` so a
/// `set-*` response can show the same row layout in text mode and
/// downstream consumers can treat the result like a single row of
/// the `annotations` listing.
#[derive(Serialize, Debug, Clone)]
pub struct AnnotationWriteResult {
    pub artifact: String,
    pub entry: AnnotationEntry,
}

/// Stringly-typed key as accepted by the write verbs. Each verb
/// parses this into an `AnnotationKey`. Kept as a single struct
/// (rather than a verb-per-key-kind explosion) so the CLI + MCP
/// surface stays compact.
pub struct AnnotationKeyArgs<'a> {
    pub kind: &'a str,
    /// For `address`: hex string (with or without 0x). For others:
    /// the raw string (symbol display name, class JNI, etc.).
    pub key: &'a str,
    /// Only used when `kind == "method"` — `key` is the class JNI,
    /// `method` is `name + descriptor` (e.g. `bar(Ljava/lang/String;)V`).
    pub method: Option<&'a str>,
}

impl<'a> AnnotationKeyArgs<'a> {
    fn parse(self) -> Result<AnnotationKey> {
        match self.kind {
            "address" => {
                let s = self.key.trim().trim_start_matches("0x").trim_start_matches("0X");
                let addr = u64::from_str_radix(s, 16)
                    .with_context(|| format!("bad hex address {:?}", self.key))?;
                Ok(AnnotationKey::Address(addr))
            }
            "symbol" => Ok(AnnotationKey::Symbol(self.key.to_string())),
            "class" => Ok(AnnotationKey::Class(self.key.to_string())),
            "method" => {
                let method = self
                    .method
                    .with_context(|| "method key requires `method` (name+descriptor)")?;
                Ok(AnnotationKey::Method(self.key.to_string(), method.to_string()))
            }
            "method-line" => {
                // `method` is required and is "name+descriptor#line"
                // — descriptor and line offset separated by '#'.
                // This is a single-arg encoding so the existing
                // CLI/MCP surface keeps the same three-field shape.
                let method = self
                    .method
                    .with_context(|| "method-line key requires `method` ('name(descriptor)return#N')")?;
                let (name_sig, line_str) = method
                    .rsplit_once('#')
                    .with_context(|| "method-line: `method` must end with #<line_offset>")?;
                let line_offset: u32 = line_str
                    .parse()
                    .with_context(|| format!("method-line: bad line offset {line_str:?}"))?;
                Ok(AnnotationKey::MethodLine(
                    self.key.to_string(),
                    name_sig.to_string(),
                    line_offset,
                ))
            }
            other => anyhow::bail!(
                "unknown key_kind {other:?}: expected one of address / symbol / class / method / method-line"
            ),
        }
    }
}

/// Set the `rename` facet. Leaves comment + colour on the same key
/// untouched.
pub fn set_rename(
    path: impl AsRef<Path>,
    key: AnnotationKeyArgs<'_>,
    new_name: &str,
) -> Result<AnnotationWriteResult> {
    merge_one(path, key, |a| {
        a.rename = Some(new_name.to_string());
    })
}

/// Set the `comment` facet. Leaves rename + colour untouched.
pub fn set_comment(
    path: impl AsRef<Path>,
    key: AnnotationKeyArgs<'_>,
    text: &str,
) -> Result<AnnotationWriteResult> {
    merge_one(path, key, |a| {
        a.comment = Some(text.to_string());
    })
}

/// Set the `colour` facet. `rgba` is parsed as `0xRRGGBBAA` —
/// 8 hex digits, with or without the `0x` prefix. Use `rgba()` not
/// `rgb()` (see memory: feedback-gpui-rgb-vs-rgba).
pub fn set_colour(
    path: impl AsRef<Path>,
    key: AnnotationKeyArgs<'_>,
    rgba: &str,
) -> Result<AnnotationWriteResult> {
    let s = rgba.trim().trim_start_matches("0x").trim_start_matches("0X");
    let colour = u32::from_str_radix(s, 16)
        .with_context(|| format!("bad RGBA hex {rgba:?} — expected 8 hex digits"))?;
    merge_one(path, key, move |a| {
        a.colour = Some(colour);
    })
}

/// Remove an annotation. No-op if the key didn't exist.
pub fn clear_annotation(
    path: impl AsRef<Path>,
    key: AnnotationKeyArgs<'_>,
) -> Result<AnnotationClearResult> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let aid = ArtifactId::from_bytes(&bytes);
    let parsed = key.parse()?;
    let db = Database::open(false).context("opening glass-db")?;
    db.clear_annotation(aid.clone(), parsed.clone());
    db.flush().context("flushing glass-db")?;
    let (key_kind, key_str) = stringify_key(&parsed);
    Ok(AnnotationClearResult {
        artifact: aid.to_string(),
        key_kind,
        key: key_str,
    })
}

#[derive(Serialize, Debug, Clone)]
pub struct AnnotationClearResult {
    pub artifact: String,
    pub key_kind: &'static str,
    pub key: String,
}

/// Read-modify-write a single annotation slot: load whatever's
/// already stored under `(aid, parsed_key)`, apply `mutate`, store.
/// Keeps the other two facets intact so `set-rename` doesn't blow
/// away an existing colour, etc.
fn merge_one(
    path: impl AsRef<Path>,
    key: AnnotationKeyArgs<'_>,
    mutate: impl FnOnce(&mut Annotation),
) -> Result<AnnotationWriteResult> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let aid = ArtifactId::from_bytes(&bytes);
    let parsed = key.parse()?;
    let db = Database::open(false).context("opening glass-db")?;
    // Find an existing value if one is stored under this key.
    let mut current = db
        .load_annotations(&aid)?
        .into_iter()
        .find(|(k, _)| k == &parsed)
        .map(|(_, v)| v)
        .unwrap_or_default();
    mutate(&mut current);
    db.set_annotation(aid.clone(), parsed.clone(), current.clone());
    db.flush().context("flushing glass-db")?;
    Ok(AnnotationWriteResult {
        artifact: aid.to_string(),
        entry: to_entry(parsed, current),
    })
}

fn stringify_key(key: &AnnotationKey) -> (&'static str, String) {
    match key {
        AnnotationKey::Address(a) => ("address", format!("0x{a:x}")),
        AnnotationKey::Symbol(s) => ("symbol", s.clone()),
        AnnotationKey::Class(c) => ("class", c.clone()),
        AnnotationKey::Method(c, m) => ("method", format!("{c}->{m}")),
        AnnotationKey::MethodLine(c, m, line) => {
            ("method-line", format!("{c}->{m}#{line}"))
        }
    }
}

fn to_entry(key: AnnotationKey, value: Annotation) -> AnnotationEntry {
    let (key_kind, key_str) = stringify_key(&key);
    AnnotationEntry {
        key_kind,
        key: key_str,
        rename: value.rename,
        comment: value.comment,
        colour: value.colour.map(|c| format!("0x{c:08x}")),
    }
}
