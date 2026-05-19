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

pub const PATCH_FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchFile {
    pub version: u32,
    pub source_path: Option<PathBuf>,
    pub edits: Vec<PatchEntry>,
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
            }
        }
    })
}
