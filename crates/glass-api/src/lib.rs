//! Glass automation API.
//!
//! The capability surface that the CLI (`glass-cli`), the scripting
//! host (`glass-script`), and — eventually — the GUI all share. Each
//! verb in `docs/AutomationAPI.md` resolves to a function in this
//! crate. The CLI's job is `argv → glass_api::* → JSON to stdout`;
//! the GUI's job is `gpui event → glass_api::* → render`.
//!
//! ## Bundle handle
//!
//! Most calls go through a [`Bundle`] handle obtained from
//! [`open`]. The handle owns parsed artifact data and caches the
//! per-query indices the GUI builds at load time (symbol map per
//! artifact, search index, xref maps). Building indices is lazy
//! on first use; subsequent calls reuse the cached version.
//!
//! ## Threading
//!
//! `Bundle` is `Send + Sync` and safe to share across worker
//! threads. The internal index caches are guarded by `RwLock` so
//! parallel queries don't fight; cache fills are serialised.

mod annotations;
mod bin_search;
mod bundle;
mod cfg;
mod dex;
mod disasm;
mod export;
mod insn_matcher;
mod insn_pattern;
mod insn_variants;
mod inspect;
mod search;
mod skills;
mod strings;
mod symbols;
mod xref;

pub use annotations::{
    annotations, clear_annotation, db_dump, set_colour, set_comment, set_rename,
    AnnotationClearResult, AnnotationEntry, AnnotationKeyArgs, AnnotationWriteResult,
    AnnotationsResult, BundleRecordView, DbDumpResult,
};
pub use bin_search::{
    build_preview, parse_pattern, scan_section, Atom, BinMatch, BinSearchResult,
    DEFAULT_GAP_MAX,
};
pub use bundle::{open, Bundle, BundleKind};
pub use export::{export_to_path, EditMap, EditPatch};
pub use cfg::{CallSiteInfo, CallsFromResult, CfgBlock, CfgEdge, CfgResult};
pub use dex::{
    ClassInfo, ClassListing, FieldInfo, FieldListing, MethodCallSite,
    MethodCallsResult, MethodInfo, MethodListing, SmaliBody,
};
pub use disasm::{decode_word, DecodeResult, DisasmListing, DisasmRow};
pub use insn_pattern::{
    compile as compile_insn_pattern, compile_to_atoms as compile_insn_atoms, InsnSearchResult,
};
pub use insn_matcher::{match_variants as match_insn_variants, MatchCandidate};
pub use insn_variants::{variants as insn_variants, SlotSpec, Variant};
pub use inspect::{
    ArtifactInfo, ArtifactKind, ArtifactSections, BinaryInfo, BundleInspection,
    SectionInfo,
};
pub use symbols::{
    demangle, DemangleResult, SymbolInfo, SymbolKindName, SymbolListing, SymbolQuery,
};
pub use search::{SearchHit, SearchResult};
pub use skills::{catalog as skill_catalog, Skill, SkillCatalog};
pub use strings::{StringHit, StringsListing};
pub use xref::{
    DexCallersResult, FieldRefsResult, XrefResult, XrefSite,
};

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;

/// Content-hash a file. Returns the artifact id + byte count +
/// elapsed wall time (lets `glass hash` double as the old
/// `hash-bench`). No bundle parsing — pure read + hash.
#[derive(serde::Serialize, Debug, Clone)]
pub struct HashResult {
    pub artifact_id: String,
    pub size_bytes: usize,
    pub duration_ms: u128,
}

pub fn hash_file(path: impl AsRef<Path>) -> Result<HashResult> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let start = Instant::now();
    let id = glass_db::ArtifactId::from_bytes(&bytes);
    let duration_ms = start.elapsed().as_millis();
    Ok(HashResult {
        artifact_id: id.to_string(),
        size_bytes: bytes.len(),
        duration_ms,
    })
}

// Re-export the underlying domain types so consumers depend on
// glass-api only, not the whole crate graph.
pub use glass_db::{ArtifactId, BundleId};
pub use glass_arch_arm64::{Symbol, SymbolKind, SymbolMap, SymbolSources};
