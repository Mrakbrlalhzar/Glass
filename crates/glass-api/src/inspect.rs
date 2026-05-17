//! Bundle inspection — `glass inspect`, `glass artifacts`, etc.
//!
//! Pure data shapes; no formatting concerns. The CLI's JSON
//! envelope wraps these; the GUI (eventually) will render them
//! directly.

use glass_db::ArtifactId;
use serde::Serialize;

use crate::bundle::{Bundle, BundleKind};

/// Top-level result of `glass inspect <path>` — the kind, label,
/// content hash, and the full artifact list with summaries.
#[derive(Serialize, Debug, Clone)]
pub struct BundleInspection {
    pub kind: String,
    pub label: String,
    pub bundle_id: Option<String>,
    pub source_path: String,
    pub artifacts: Vec<ArtifactInfo>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ArtifactInfo {
    pub id: String,
    pub label: String,
    pub kind: ArtifactKind,
    /// Total binary bytes (post fat-slicing).
    pub size_bytes: usize,
    /// Architecture as reported by armv8-encode.
    pub architecture: String,
    /// Number of sections in the container.
    pub section_count: usize,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    /// AArch64 ELF / Mach-O (text section disassemblable).
    Native,
    /// Anything else (placeholder — future DEX / odex / 32-bit ARM
    /// artifacts come through here).
    Other,
}

impl Bundle {
    /// Full inspection summary. Cheap — no index builds, just
    /// metadata already parsed when [`open`] ran.
    pub fn inspect(&self) -> BundleInspection {
        let kind = match self.kind() {
            BundleKind::Apk => "apk",
            BundleKind::Ipa => "ipa",
            BundleKind::Native => "native",
        }
        .to_string();
        let bundle_id = bundle_id_for(self);
        let artifacts = self
            .artifacts
            .iter()
            .map(|a| ArtifactInfo {
                id: a.id.to_string(),
                label: a.label.clone(),
                kind: ArtifactKind::Native,
                size_bytes: a.binary.bytes.len(),
                architecture: format!("{:?}", a.binary.container.architecture),
                section_count: a.binary.container.sections.len(),
            })
            .collect();
        BundleInspection {
            kind,
            label: self.label.clone(),
            bundle_id,
            source_path: self.source_path.display().to_string(),
            artifacts,
        }
    }

    /// Just the artifact list, no bundle-level metadata.
    pub fn artifacts(&self) -> Vec<ArtifactInfo> {
        self.inspect().artifacts
    }
}

/// Compute the bundle id the way the GUI's loader does: blake3 of
/// the concatenated artifact-ids. Bare-native bundles return None
/// because there's only one artifact and the bundle id would just
/// duplicate it.
fn bundle_id_for(bundle: &Bundle) -> Option<String> {
    if bundle.kind() == BundleKind::Native || bundle.artifacts.is_empty() {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    for a in &bundle.artifacts {
        hasher.update(a.id.as_bytes());
    }
    let raw = *hasher.finalize().as_bytes();
    Some(glass_db::BundleId::from_raw(raw).to_string())
}

// ---- Sections --------------------------------------------------------------

#[derive(Serialize, Debug, Clone)]
pub struct ArtifactSections {
    pub artifact: String,
    pub sections: Vec<SectionInfo>,
}

#[derive(Serialize, Debug, Clone)]
pub struct SectionInfo {
    pub name: String,
    pub address: String,
    pub size: u64,
    pub kind: String,
    pub bytes_on_disk: usize,
}

impl Bundle {
    /// Section table for one artifact, or all artifacts when
    /// `artifact_filter` is None. `artifact_filter` is matched by
    /// label or by ArtifactId hex-prefix (see `resolve_artifact`).
    pub fn sections(&self, artifact_filter: Option<&str>) -> Vec<ArtifactSections> {
        let mut out = Vec::new();
        for a in &self.artifacts {
            if let Some(needle) = artifact_filter {
                if a.label != needle && !a.id.to_string().starts_with(needle) {
                    continue;
                }
            }
            let sections = a
                .binary
                .container
                .sections
                .iter()
                .map(|s| SectionInfo {
                    name: s.name.clone(),
                    address: format!("0x{:x}", s.address),
                    size: s.size,
                    kind: format!("{:?}", s.kind),
                    bytes_on_disk: s.bytes.len(),
                })
                .collect();
            out.push(ArtifactSections {
                artifact: a.id.to_string(),
                sections,
            });
        }
        out
    }
}

// ---- Binary info -----------------------------------------------------------

#[derive(Serialize, Debug, Clone)]
pub struct BinaryInfo {
    pub artifact: String,
    pub label: String,
    pub architecture: String,
    pub format: String,
    pub size_bytes: usize,
    pub symbol_count_hint: usize,
    pub section_count: usize,
}

impl Bundle {
    /// Per-artifact summary, deeper than `inspect()`: format
    /// (ELF / Mach-O), architecture, raw counts. Useful for
    /// "is this binary worth disassembling?" scripts.
    pub fn binary_info(&self) -> Vec<BinaryInfo> {
        self.artifacts
            .iter()
            .map(|a| {
                let format = if a.binary.container.elf_image.is_some() {
                    "elf"
                } else if a.binary.container.macho_image.is_some() {
                    "macho"
                } else {
                    "unknown"
                };
                BinaryInfo {
                    artifact: a.id.to_string(),
                    label: a.label.clone(),
                    architecture: format!("{:?}", a.binary.container.architecture),
                    format: format.to_string(),
                    size_bytes: a.binary.bytes.len(),
                    symbol_count_hint: a.binary.container.symbols.len(),
                    section_count: a.binary.container.sections.len(),
                }
            })
            .collect()
    }
}

// Suppress dead-code on the helper used by future verbs.
#[allow(dead_code)]
fn _used_indirectly(id: &ArtifactId) -> &ArtifactId {
    id
}
