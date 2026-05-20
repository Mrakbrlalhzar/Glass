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
mod insn_cursor;
mod insn_matcher;
mod insn_pattern;
mod insn_variants;
mod patch_file;
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
pub use bundle::{open, Bundle, BundleKind, DexGroup};
pub use export::{
    export_to_path, export_to_path_with_smali, EditMap, EditPatch, SmaliEditMap,
};
pub use patch_file::{
    schema as patch_file_schema, PatchEntry, PatchFile, PatchKind, SmaliPatchEntry,
    PATCH_FILE_VERSION,
};

thread_local! {
    /// When true, the active panic hook chain should suppress its
    /// normal output (backtrace, stderr write). Used by
    /// `parse_smali_class` to silence panics from the smali op
    /// parser, which the live op editor triggers on every
    /// keystroke against partial input. Other panics on this
    /// thread fall through to whatever hook is installed
    /// elsewhere.
    pub static SUPPRESS_PANIC_OUTPUT: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Install a panic hook that swallows panic output when
/// `SUPPRESS_PANIC_OUTPUT` is set on the panicking thread, and
/// delegates to `default_hook` (or whatever previous hook was
/// installed) otherwise. Call this once at app startup so live
/// editors can safely call `parse_smali_class` without flooding
/// stderr with backtraces from caught panics.
///
/// Composable: chains to `prev` so existing crash reporters
/// (gpui's, the CLI's, etc.) still fire on real panics.
pub fn install_suppressible_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let suppressed =
            SUPPRESS_PANIC_OUTPUT.with(|cell| cell.get());
        if !suppressed {
            prev(info);
        }
    }));
}

/// Parse a smali class body. Thin wrapper so CLI / MCP callers
/// don't need a direct dep on the `smali` crate.
///
/// The underlying parser (`smali::smali_ops::parse_op` and a
/// handful of operand decoders) panics on malformed input —
/// e.g. an `.line` with no number, an op with a trailing-bare
/// `v`. Live editor flows feed it half-typed buffers on every
/// keystroke, so we catch the unwind and translate the panic
/// message into an `anyhow` error. Pair this with
/// `install_suppressible_panic_hook` at app startup to keep
/// stderr quiet on caught panics.
pub fn parse_smali_class(body: &str) -> anyhow::Result<smali::types::SmaliClass> {
    let body = body.to_string();
    // Mark this thread so the panic hook (installed at app
    // startup via `install_suppressible_panic_hook`) suppresses
    // any output from the smali parser's panicking on
    // partial-input lines. We still catch the unwind below and
    // return a normal `Err`; the hook just stops the backtrace
    // from being splattered onto stderr.
    SUPPRESS_PANIC_OUTPUT.with(|cell| cell.set(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        smali::types::SmaliClass::from_smali(&body)
    }));
    SUPPRESS_PANIC_OUTPUT.with(|cell| cell.set(false));
    match result {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(anyhow::anyhow!("parsing smali body: {e:?}")),
        Err(panic_payload) => {
            // The panic message is the most informative thing
            // we have. `panic_payload` is `Box<dyn Any + Send>`
            // — `&str` and `String` are the common payload
            // shapes, plus the formatted message gpui's panic
            // hook produces.
            let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "smali parser panicked (no message)".to_string()
            };
            Err(anyhow::anyhow!("parsing smali body: {msg}"))
        }
    }
}

/// JNI signature of a parsed smali class — used by `smali-set`
/// to verify the body's `.class` line matches the slot it's
/// targeting.
pub fn smali_class_jni(class: &smali::types::SmaliClass) -> String {
    class.name.as_jni_type()
}

#[cfg(test)]
mod parse_smali_class_tests {
    use super::*;

    /// Mid-typed register operand makes the smali parser panic.
    /// We must catch the unwind and surface it as an error so
    /// live editors can flash the message instead of taking the
    /// process down.
    #[test]
    fn catches_op_parser_panic_on_partial_register() {
        let body = "\
.class public Lglass/internal/T;
.super Ljava/lang/Object;
.method public foo()V
    const/4  v
.end method
";
        let res = parse_smali_class(body);
        assert!(res.is_err(), "expected Err, got Ok");
        let msg = format!("{:#}", res.unwrap_err());
        assert!(msg.contains("parsing smali body"), "msg = {msg}");
    }
}
pub use cfg::{CallSiteInfo, CallsFromResult, CfgBlock, CfgEdge, CfgResult};
pub use dex::{
    ClassInfo, ClassListing, FieldInfo, FieldListing, MethodCallSite,
    MethodCallsResult, MethodInfo, MethodListing, SmaliBody,
};
pub use disasm::{decode_word, DecodeResult, DisasmListing, DisasmRow};
pub use insn_pattern::{
    compile as compile_insn_pattern, compile_at as compile_insn_at,
    compile_to_atoms as compile_insn_atoms, InsnSearchResult,
};
pub use insn_cursor::{classify as classify_insn_cursor, CursorContext, CursorKind};
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
