//! Persistent patch-file format for `glass patch` / `glass
//! export-patched`. Mirrors the GUI's in-memory `EditRegistry`
//! but uses a serialisable shape so edits can be accumulated
//! across CLI / MCP invocations and shared between sessions.
//!
//! File layout (JSON):
//!
//! ```json
//! {
//!   "version": 1,
//!   "source_path": "/abs/path/to/bundle.apk",
//!   "edits": [
//!     {
//!       "artifact": "<64-char hex artifact id>",
//!       "vaddr": 4294967296,
//!       "kind": "Instruction",
//!       "new_bytes": [0x20, 0x00, 0x80, 0x52],
//!       "original_bytes": [0xc0, 0x03, 0x5f, 0xd6],
//!       "source_text": "mov w0, #1"
//!     }
//!   ]
//! }
//! ```
//!
//! - `version` — bump when the format changes incompatibly.
//! - `source_path` — informational; the bundle the patches
//!   were built against. The exporter doesn't require it to
//!   match (so you can pass a relocated copy of the same
//!   bytes) but warns if it does and the artifact ids don't
//!   appear in the supplied bundle.
//! - `edits` — flat list. Order doesn't matter; the exporter
//!   groups by artifact internally. Same `(artifact, vaddr)`
//!   appearing twice means the later entry wins (matches
//!   `EditRegistry::insert` semantics).
//! - `kind` — `"Instruction"`, `"Bytes"`, or `"String"`.
//!   Display-only — the splice writes `new_bytes` regardless
//!   of kind.
//! - `original_bytes` — captured at stage time. Lets a viewer
//!   show "was → now" without re-reading the source bundle.
//!   Optional on input (ignored if absent); the writer always
//!   emits it for forward-compat.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::export::{EditMap, EditPatch};
use glass_db::ArtifactId;

/// File-format version. Bumped on incompatible additions.
///
/// v1: byte-level `edits` only.
/// v2: adds `smali_edits` for typed DEX class rewrites. v1 files
///     still parse (the new field defaults to empty); writers
///     emit v2 unconditionally.
pub const PATCH_FILE_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchFile {
    pub version: u32,
    pub source_path: Option<PathBuf>,
    pub edits: Vec<PatchEntry>,
    /// Typed smali-class rewrites. Each entry replaces the
    /// matching class in the DEX identified by `artifact`. The
    /// exporter re-emits the entire DEX via `DexFile::from_smali`
    /// — see `export::SmaliEditMap`.
    #[serde(default)]
    pub smali_edits: Vec<SmaliPatchEntry>,
}

/// One typed smali class replacement.
///
/// `body` is the full smali text for the class (everything you'd
/// get from `glass smali --class ...`). Parsed at export time via
/// `smali::types::SmaliClass::from_smali`; a malformed body is
/// rejected with a diagnostic rather than silently skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmaliPatchEntry {
    /// 64-char hex artifact id of the DEX (`classes.dex` /
    /// `classes2.dex` / …) that owns this class.
    pub artifact: String,
    /// JNI signature of the class — `Lcom/example/Foo;`.
    pub class_jni: String,
    /// Full smali body of the class.
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchEntry {
    pub artifact: String,
    pub vaddr: u64,
    pub kind: PatchKind,
    pub new_bytes: Vec<u8>,
    #[serde(default)]
    pub original_bytes: Vec<u8>,
    #[serde(default)]
    pub source_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PatchKind {
    Instruction,
    Bytes,
    String,
}

impl Default for PatchFile {
    fn default() -> Self {
        Self {
            version: PATCH_FILE_VERSION,
            source_path: None,
            edits: Vec::new(),
            smali_edits: Vec::new(),
        }
    }
}

impl PatchFile {
    /// Read a patch file from disk. Returns an empty file when
    /// `path` doesn't exist, so the first `glass patch` call
    /// against a fresh path doesn't have to special-case.
    pub fn read_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading patch file {}", path.display()))?;
        let pf: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing patch file {}", path.display()))?;
        if pf.version > PATCH_FILE_VERSION {
            anyhow::bail!(
                "patch file version {} is newer than this build ({})",
                pf.version,
                PATCH_FILE_VERSION
            );
        }
        Ok(pf)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating parent {}", parent.display())
                })?;
            }
        }
        let bytes = serde_json::to_vec_pretty(self)
            .context("serialising patch file")?;
        std::fs::write(path, bytes)
            .with_context(|| format!("writing patch file {}", path.display()))?;
        Ok(())
    }

    /// Add or replace the entry for `(artifact, vaddr)`. Same
    /// last-write-wins semantics as `EditRegistry::insert`.
    pub fn upsert(&mut self, entry: PatchEntry) {
        if let Some(slot) = self
            .edits
            .iter_mut()
            .find(|e| e.artifact == entry.artifact && e.vaddr == entry.vaddr)
        {
            *slot = entry;
        } else {
            self.edits.push(entry);
        }
    }

    /// Remove the entry for `(artifact, vaddr)` if any.
    pub fn remove(&mut self, artifact: &str, vaddr: u64) -> bool {
        let before = self.edits.len();
        self.edits.retain(|e| !(e.artifact == artifact && e.vaddr == vaddr));
        self.edits.len() != before
    }

    /// Add or replace the smali entry for `(artifact, class_jni)`.
    pub fn upsert_smali(&mut self, entry: SmaliPatchEntry) {
        if let Some(slot) = self.smali_edits.iter_mut().find(|e| {
            e.artifact == entry.artifact && e.class_jni == entry.class_jni
        }) {
            *slot = entry;
        } else {
            self.smali_edits.push(entry);
        }
    }

    /// Remove the smali entry for `(artifact, class_jni)` if any.
    pub fn remove_smali(&mut self, artifact: &str, class_jni: &str) -> bool {
        let before = self.smali_edits.len();
        self.smali_edits
            .retain(|e| !(e.artifact == artifact && e.class_jni == class_jni));
        self.smali_edits.len() != before
    }

    /// Build a [`SmaliEditMap`](crate::export::SmaliEditMap) by
    /// parsing each body via `SmaliClass::from_smali`. Returns the
    /// first parse / artifact-id error rather than skipping —
    /// silently dropping a typed edit at export time would be
    /// confusing.
    pub fn to_smali_edit_map(&self) -> Result<crate::export::SmaliEditMap> {
        let mut map: crate::export::SmaliEditMap = std::collections::HashMap::new();
        for e in &self.smali_edits {
            let aid = parse_artifact_id(&e.artifact).with_context(|| {
                format!(
                    "smali edit for {}: bad artifact id {:?}",
                    e.class_jni, e.artifact
                )
            })?;
            let class = smali::types::SmaliClass::from_smali(&e.body).map_err(|err| {
                anyhow::anyhow!(
                    "smali edit for {} on artifact {}: parse failed: {err:?}",
                    e.class_jni,
                    e.artifact
                )
            })?;
            // Defend against the body declaring a different class
            // than the entry advertises — without this check, a
            // typo lets you silently overwrite the wrong class.
            let body_jni = class.name.as_jni_type();
            if body_jni != e.class_jni {
                anyhow::bail!(
                    "smali edit body declares class {body_jni:?} but entry says {:?}",
                    e.class_jni
                );
            }
            map.entry(aid).or_default().insert(e.class_jni.clone(), class);
        }
        Ok(map)
    }

    /// Build an `EditMap` (the export pipeline's input) from
    /// the entries. Unparseable artifact ids are skipped with a
    /// warning so a malformed entry can't block the whole
    /// export.
    pub fn to_edit_map(&self) -> EditMap {
        let mut map: EditMap = std::collections::HashMap::new();
        for e in &self.edits {
            match parse_artifact_id(&e.artifact) {
                Ok(aid) => map.entry(aid).or_default().push(EditPatch {
                    vaddr: e.vaddr,
                    new_bytes: e.new_bytes.clone(),
                }),
                Err(err) => {
                    tracing::warn!(
                        "patch file entry skipped: bad artifact id {:?}: {err:#}",
                        e.artifact
                    );
                }
            }
        }
        map
    }
}

/// Parse a 64-char hex string as an `ArtifactId`. The Display
/// impl on ArtifactId only shows the first 8 chars + ellipsis;
/// we keep the full 64-char form in the patch file so it
/// round-trips losslessly.
pub(crate) fn parse_artifact_id(s: &str) -> Result<ArtifactId> {
    if s.len() != 64 {
        anyhow::bail!("expected 64-char hex string, got {} chars", s.len());
    }
    let mut raw = [0u8; 32];
    for (i, byte) in raw.iter_mut().enumerate() {
        let from = i * 2;
        *byte = u8::from_str_radix(&s[from..from + 2], 16)
            .with_context(|| format!("non-hex at byte {i}"))?;
    }
    Ok(ArtifactId::from_raw(raw))
}

/// JSON Schema (draft 2020-12) describing the patch-file
/// format. Hand-written rather than derived so the field
/// descriptions are tuned for the `patch-schema` CLI verb and
/// any external consumer that wants to validate.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Glass patch file",
        "description": "Accumulated edits ready to be applied to a bundle by `glass export-patched`.",
        "type": "object",
        "required": ["version", "edits"],
        "properties": {
            "version": {
                "type": "integer",
                "const": PATCH_FILE_VERSION,
                "description": "File-format version. Bumped on incompatible changes."
            },
            "source_path": {
                "type": ["string", "null"],
                "description": "Informational: bundle path the patches were built against."
            },
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["artifact", "vaddr", "kind", "new_bytes"],
                    "properties": {
                        "artifact": {
                            "type": "string",
                            "description": "Content-hash artifact id (64-char hex) — same form as `glass inspect` reports."
                        },
                        "vaddr": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Virtual address where the splice begins."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["Instruction", "Bytes", "String"],
                            "description": "Display-only: how the original edit was made. Splice writes new_bytes regardless."
                        },
                        "new_bytes": {
                            "type": "array",
                            "items": { "type": "integer", "minimum": 0, "maximum": 255 },
                            "description": "Bytes to splice in. Must be the same length as the original at vaddr."
                        },
                        "original_bytes": {
                            "type": "array",
                            "items": { "type": "integer", "minimum": 0, "maximum": 255 },
                            "description": "Bytes that were there before, captured at stage time. Optional."
                        },
                        "source_text": {
                            "type": "string",
                            "description": "What the user originally typed (e.g. 'mov w0, #1' or 'hello world'). Optional."
                        }
                    }
                }
            },
            "smali_edits": {
                "type": "array",
                "description": "Typed smali class rewrites. Each entry replaces the matching class in the named DEX artifact at `export-patched` time.",
                "items": {
                    "type": "object",
                    "required": ["artifact", "class_jni", "body"],
                    "properties": {
                        "artifact": {
                            "type": "string",
                            "description": "64-char hex DEX artifact id (`classes.dex` / `classes2.dex` / …)."
                        },
                        "class_jni": {
                            "type": "string",
                            "description": "JNI signature of the class, e.g. `Lcom/example/Foo;`. Must match the class declared in `body`."
                        },
                        "body": {
                            "type": "string",
                            "description": "Full smali text for the class. Same shape `glass smali --class …` returns."
                        }
                    }
                }
            }
        }
    })
}
