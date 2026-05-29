//! Cross-reference indices.
//!
//! Three indices, each built on a background thread after the
//! foreground bundle load completes so first paint stays responsive:
//!
//!   * `native` — for every native (AArch64) artifact, a map from
//!     `target_addr` → list of caller-site addresses. Caller sites
//!     are direct branch instructions plus resolved ADRP+ADD pairs.
//!   * `dex_callers` — inverse of `bundle.method_calls`. Methods
//!     that invoke each key.
//!   * `dex_field_refs` — for every field reference seen in any
//!     smali body, the list of method keys that touch it
//!     (iget/iput/sget/sput).
//!
//! Each index is wrapped in an `XrefIndexState` so the UI can show a
//! progress bar while the build is in flight. The store is owned by
//! `LoadedBundle` and shared into the build tasks via Arc.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

/// One in-flight xref index. The Shell renders a progress chip when
/// `state == Building` and the real results panel when `state ==
/// Ready`. `Failed` is defensive — none of the builders are
/// fallible today but the state machine accommodates them.
#[derive(Clone)]
#[derive(Default)]
pub enum XrefIndexState<T> {
    /// Build hasn't started yet. The Shell shouldn't see this in
    /// practice (the loader fires builders immediately after bundle
    /// hand-off), but it's the cheap initial value.
    #[default]
    Pending,
    /// Build is running. The `XrefProgress` is shared with the
    /// worker so the UI can render `current / total`.
    Building(Arc<parking_lot::Mutex<XrefProgress>>),
    /// Build finished; result is Arc'd so cloning is cheap.
    Ready(Arc<T>),
    /// Build failed. Stores a short message for diagnostics.
    #[allow(dead_code)]
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct XrefProgress {
    /// Free-text label rendered above the progress bar.
    pub label: String,
    /// Items processed so far.
    pub current: usize,
    /// Total items the build will process. May be approximate; the
    /// bar clamps so it never overflows visually.
    pub total: usize,
}

impl XrefProgress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.;
        }
        (self.current as f32 / self.total as f32).clamp(0., 1.)
    }
}

/// Inverse call graph for a native artifact: target addr →
/// caller-site addrs.
pub type NativeXrefMap = HashMap<u64, Vec<u64>>;

/// Per-artifact native xref maps. Same keying as `LoadedBundle.symbol_maps`.
pub type NativeXrefs = HashMap<glass_db::ArtifactId, NativeXrefMap>;

/// `caller_key` + the line offset within that method body where
/// the `invoke-*` lives, for each `callee_key`. Multiple entries
/// per caller when one method invokes the same callee on several
/// lines. Sorted on `(caller_key, line_offset)` for stable order.
pub type DexCallers = HashMap<String, Vec<(String, u32)>>;

/// For each smali field reference (`Lcom/Foo;->name:Ltype;`), the
/// `(method_key, line_offset)` pairs that touch it. Same sort
/// shape as `DexCallers`.
pub type DexFieldRefs = HashMap<String, Vec<(String, u32)>>;

/// What an in-flight scoped palette is querying. Stored on the
/// scope so the poller can rebuild entries deterministically when
/// the underlying index transitions to Ready.
#[derive(Clone, Debug)]
pub enum PaletteScopeSource {
    /// `target_addr` references in the native xref index for
    /// `artifact`.
    NativeXrefs { artifact: glass_db::ArtifactId, target_addr: u64 },
    /// Callers of a DEX method.
    DexCallers { method_key: String },
    /// Touchers of a DEX field.
    DexFieldRefs { field_ref: String },
}

/// A scoped palette query — populates the palette with a fixed set
/// of `SearchEntry` results instead of the bundle-wide index.
///
/// The header chip reads "Label — N results" (or "Label — indexing
/// …" while the underlying index is still building). The Shell
/// constructs one of these when the user picks "References to X" or
/// "Callers of X" from a right-click menu.
#[derive(Clone)]
pub struct PaletteScope {
    /// Free-text label rendered in the chip above the palette input.
    pub label: String,
    /// Pre-computed entries the palette filters within. May be
    /// empty while `progress` is still in flight; the Shell refreshes
    /// when the underlying index becomes Ready.
    pub entries: Arc<Vec<crate::SearchEntry>>,
    /// `Some` while the producing index is still building. The
    /// palette polls this to render a bar; when None, the scope is
    /// terminal (results are final).
    pub progress: Option<Arc<parking_lot::Mutex<XrefProgress>>>,
    /// What this scope is querying. Used by the background poller
    /// to rebuild `entries` once the source index transitions to
    /// Ready.
    pub source: PaletteScopeSource,
}

/// The full xref store. Owned by `LoadedBundle`; cloning is cheap
/// because everything is Arc'd.
#[derive(Clone, Default)]
pub struct XrefStore {
    pub native: Arc<RwLock<XrefIndexState<NativeXrefs>>>,
    pub dex_callers: Arc<RwLock<XrefIndexState<DexCallers>>>,
    pub dex_field_refs: Arc<RwLock<XrefIndexState<DexFieldRefs>>>,
}

impl XrefStore {
    pub fn new() -> Self {
        Self::default()
    }
}


impl<T> XrefIndexState<T> {
    /// Convenience: whether any consumer has the finished index in
    /// hand. Used by the Shell to enable / disable right-click menu
    /// items.
    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        matches!(self, XrefIndexState::Ready(_))
    }
}

// ---- AArch64 builder -------------------------------------------------------

/// Walk every native text section, decode every instruction, and
/// build the per-artifact `target_addr → caller_sites` index.
/// Handles direct branches via `primary_address_operand` plus
/// resolved ADRP+ADD pairs via the same page-base tracker used in
/// `build_listing_rows`.
pub fn build_native_xrefs(
    text_sections: &std::collections::HashMap<
        (glass_db::ArtifactId, String),
        crate::TextSectionBytes,
    >,
    progress: &Arc<parking_lot::Mutex<XrefProgress>>,
) -> NativeXrefs {
    use armv8_encode::isa::aarch64;
    use glass_arch_arm::{DecodedInsn, PageBaseTracker};
    let mut out: NativeXrefs = HashMap::new();
    let mut processed_total = 0usize;
    for ((aid, _name), section) in text_sections {
        let base = section.base;
        let bytes: &[u8] = section.bytes.as_ref();
        let n = bytes.len() / 4;
        let per_artifact = out.entry(aid.clone()).or_default();
        // Shared fusion tracker — same idioms as `build_listing_rows`.
        let mut tracker = PageBaseTracker::new();
        for i in 0..n {
            let addr = base + (i as u64) * 4;
            let word = u32::from_le_bytes([
                bytes[i * 4],
                bytes[i * 4 + 1],
                bytes[i * 4 + 2],
                bytes[i * 4 + 3],
            ]);
            let Ok(insn) = aarch64::decode_instruction(addr, word) else {
                if i % 1024 == 0 {
                    let mut p = progress.lock();
                    p.current = processed_total + i;
                }
                continue;
            };
            // Direct branch target → xref.
            if let Some(target) =
                glass_arch_arm::format::primary_address_operand(&insn)
            {
                per_artifact.entry(target).or_default().push(addr);
            }
            // ADRP+ADD fusion via the shared tracker.
            let wrapped = DecodedInsn::Aarch64(insn);
            if let Some(ft) = tracker.observe(&wrapped) {
                per_artifact.entry(ft.target).or_default().push(addr);
            }
            // Progress at 1024-insn cadence to keep lock contention low.
            if i % 1024 == 0 {
                let mut p = progress.lock();
                p.current = processed_total + i;
            }
        }
        processed_total += n;
        let mut p = progress.lock();
        p.current = processed_total;
    }
    // Make caller lists deterministic for stable display ordering.
    for v in out.values_mut() {
        for sites in v.values_mut() {
            sites.sort_unstable();
            sites.dedup();
        }
    }
    out
}

// ---- DEX field refs builder -----------------------------------------------

/// Scan every smali class body for field accesses
/// (`iget*/iput*/sget*/sput*`) and build `field_ref → method_keys`.
/// The method key is the same `Class;->name(sig)ret` form used by
/// `method_calls`. The field ref is the last whitespace-separated
/// token on the instruction line, in the same form as smali source
/// (`Lcom/Foo;->name:Ltype;`).
pub fn build_dex_field_refs(
    bodies: &[gpui::SharedString],
    kinds: &[crate::LeafKind],
    progress: &Arc<parking_lot::Mutex<XrefProgress>>,
) -> DexFieldRefs {
    let mut out: DexFieldRefs = HashMap::new();
    let mut processed = 0usize;
    for (i, k) in kinds.iter().enumerate() {
        let crate::LeafKind::SmaliClass { class_jni } = k else { continue };
        processed += 1;
        let Some(body) = bodies.get(i) else { continue };
        // Track current `.method` so we can record the line offset
        // relative to its header (matches the MethodLine annotation
        // key convention).
        let mut current_method: Option<String> = None;
        let mut method_line_idx: usize = 0;
        for (line_no, raw) in body.lines().enumerate() {
            let trimmed = raw.trim_start();
            if let Some(after) = trimmed.strip_prefix(".method ") {
                if let Some(decl) = after.split_whitespace().last() {
                    current_method = Some(format!("{class_jni}->{decl}"));
                    method_line_idx = line_no;
                }
                continue;
            }
            if trimmed.starts_with(".end method") {
                current_method = None;
                continue;
            }
            // Looking for iget*/iput*/sget*/sput*. All start with
            // one of those four prefixes followed by a possible
            // type suffix (-boolean, -wide, etc.).
            let is_field_op = trimmed.starts_with("iget")
                || trimmed.starts_with("iput")
                || trimmed.starts_with("sget")
                || trimmed.starts_with("sput");
            if !is_field_op {
                continue;
            }
            let Some(method_key) = current_method.as_ref() else { continue };
            let Some(field_ref) = trimmed.split_whitespace().last() else {
                continue;
            };
            if !field_ref.contains("->") || !field_ref.contains(':') {
                continue;
            }
            let offset = (line_no - method_line_idx) as u32;
            out.entry(field_ref.to_string())
                .or_default()
                .push((method_key.clone(), offset));
        }
        if processed % 64 == 0 {
            let mut p = progress.lock();
            p.current = processed;
        }
    }
    for sites in out.values_mut() {
        sites.sort_unstable();
        sites.dedup();
    }
    let mut p = progress.lock();
    p.current = processed;
    out
}

/// Build the DEX callers index by scanning smali bodies. Captures
/// the line offset of each `invoke-*` so the palette entry can
/// jump to the exact call site. Replaces the cheaper-but-line-
/// less inverse-of-method_calls path that lived in app.rs.
pub fn build_dex_callers(
    bodies: &[gpui::SharedString],
    kinds: &[crate::LeafKind],
    progress: &Arc<parking_lot::Mutex<XrefProgress>>,
) -> DexCallers {
    let mut out: DexCallers = HashMap::new();
    let mut processed = 0usize;
    for (i, k) in kinds.iter().enumerate() {
        let crate::LeafKind::SmaliClass { class_jni } = k else { continue };
        processed += 1;
        let Some(body) = bodies.get(i) else { continue };
        let mut current_method: Option<String> = None;
        let mut method_line_idx: usize = 0;
        for (line_no, raw) in body.lines().enumerate() {
            let trimmed = raw.trim_start();
            if let Some(after) = trimmed.strip_prefix(".method ") {
                if let Some(decl) = after.split_whitespace().last() {
                    current_method = Some(format!("{class_jni}->{decl}"));
                    method_line_idx = line_no;
                }
                continue;
            }
            if trimmed.starts_with(".end method") {
                current_method = None;
                continue;
            }
            if !trimmed.starts_with("invoke-") {
                continue;
            }
            let Some(caller_key) = current_method.as_ref() else { continue };
            // Smali invoke syntax: `invoke-... {regs}, Callee;->name(sig)ret`.
            // The callee is the last whitespace-separated token.
            let Some(callee_key) = trimmed.split_whitespace().last() else {
                continue;
            };
            if !callee_key.contains("->") {
                continue;
            }
            let offset = (line_no - method_line_idx) as u32;
            out.entry(callee_key.to_string())
                .or_default()
                .push((caller_key.clone(), offset));
        }
        if processed % 64 == 0 {
            let mut p = progress.lock();
            p.current = processed;
        }
    }
    for sites in out.values_mut() {
        sites.sort_unstable();
        sites.dedup();
    }
    let mut p = progress.lock();
    p.current = processed;
    out
}

