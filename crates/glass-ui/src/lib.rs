//! glass-ui: minimal GPUI shell.
//!
//! Single-file UI: window, two-pane layout, virtualized tree on the left,
//! pre-rendered body text on the right. Tree groups APK content as:
//!     classes.dex
//!       com.example.foo
//!         MainActivity
//!         Utils
//!     lib/arm64-v8a
//!       libfoo.so
//!
//! When this grows past ~600 lines or a hex view / command palette lands,
//! split into separate modules.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::{
    Bounds, Context, FocusHandle, ListAlignment, ListOffset, ListState, Pixels,
    Render, SharedString, Window, actions, div, prelude::*,
    px, rgb,
};

mod about;
mod annotations;
mod annotations_pane;
mod app;
mod cfg_block;
mod cfg_render;
mod colour_picker;
mod context_menu;
mod dex_callgraph;
mod dex_cg_render;
mod graph;
mod graph_canvas;
mod debug_dock;
mod hooks;
mod hooks_dialog;
mod traces;
mod traces_dialog;
mod device_picker;
mod hex;
mod icons;
mod injection_dialog;
mod listing_model;
mod listing_render;
mod loader;
mod manifest;
mod palette;
mod changes_dialog;
mod checkbox;
mod edits;
mod string_edit_popover;
mod scrollbar;
mod search;
mod text_input;
mod section_map;
mod annotation_popover;
mod class_decl_popover;
mod external_editor;
mod field_popover;
mod method_popover;
mod modifier_picker;
mod op_editor;
mod smali_row_scope;
mod shell_actions;
mod shell_render;
mod smali;
mod smali_edits;
mod theme;
mod two_pane;
mod xref;

pub use annotations::AnnotationIndex;
pub use app::launch;
use context_menu::ContextMenuState;
use dex_callgraph::DexCallGraphState;
pub use loader::snapshot_arm64;
pub use search::{build_search_index, SearchEntry, SearchIndex, SearchJump};
pub use xref::{PaletteScope, PaletteScopeSource, XrefIndexState, XrefProgress, XrefStore};

pub use hex::{build_hex_rows, hex_row_for_addr, HexRow};
pub use icons::IconAssets;
pub use listing_model::{
    build_listing_rows, listing_row_for_addr, ArrowDirection, ArrowRole, ArrowSegment, ArrowStyle,
    DataPeek, DataSectionMeta, ListingRow, ARROW_MAX_LANES,
};
use listing_render::LISTING_ROW_HEIGHT;
pub use manifest::{flatten_info_plist, flatten_manifest, ManifestRow};

actions!(
    glass,
    [
        TogglePalette,
        PaletteClose,
        PaletteUp,
        PaletteDown,
        PaletteActivate,
        ListingPageUp,
        ListingPageDown,
        HexCursorLeft,
        HexCursorRight,
        PaletteModeText,
        PaletteModeBinary,
        PaletteAsmTab,
        ToggleChangesDialog,
        OpenFile,
        CloseFile,
        NewWindow,
        CloseWindow,
        Quit,
        About,
        // Up to 10 recent-bundle slots. Each is a zero-sized action
        // wired to a separate handler that opens index N from the
        // recent list. Avoids needing serde-deriving payload actions
        // (gpui supports them but requires schemars + JSON deser
        // setup).
        OpenRecent0,
        OpenRecent1,
        OpenRecent2,
        OpenRecent3,
        OpenRecent4,
        OpenRecent5,
        OpenRecent6,
        OpenRecent7,
        OpenRecent8,
        OpenRecent9,
        // Up to 8 theme slots — same trick as OpenRecent: separate
        // zero-sized actions because gpui's payload actions need
        // additional setup. Theme list is read from `ThemeSet::load()`
        // at menu-build time; selecting one switches the active theme
        // for every Shell window.
        Theme0,
        Theme1,
        Theme2,
        Theme3,
        Theme4,
        Theme5,
        Theme6,
        Theme7,
        // Class-declaration popover. Open via double-click on the
        // `.class` line; commit/cancel via Enter/Esc inside the
        // popover. Dispatched explicitly because the popover's
        // text inputs swallow keys before any window-level key
        // binding has a chance to fire.
        ClassDeclCommit,
        ClassDeclCancel,
        FieldCommit,
        FieldCancel,
        MethodCommit,
        MethodCancel,
    ]
);


#[derive(Debug, Clone)]
pub struct Progress {
    pub label: String,
    pub phase: SharedString,
    pub current: usize,
    pub total: usize,
    pub done: bool,
}

impl Progress {
    pub(crate) fn starting(path: &std::path::Path) -> Self {
        Self {
            label: path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("(bundle)")
                .to_string(),
            phase: SharedString::from("Opening…"),
            current: 0,
            total: 0,
            done: false,
        }
    }
}

pub(crate) enum ShellState {
    Empty,
    Loading,
    Ready(LoadedBundle),
    Error(String),
}

// ---- snapshots --------------------------------------------------------------

#[derive(Clone)]
pub struct LoadedBundle {
    pub title: String,
    pub tree: Arc<Tree>,
    /// Pre-rendered bodies, keyed by `LeafId`.
    pub bodies: Arc<Vec<SharedString>>,
    /// Subtitle for each leaf (e.g. "classes.dex" or "lib/arm64-v8a").
    pub origins: Arc<Vec<SharedString>>,
    /// Short label for each leaf — used as the tab title. For DEX classes
    /// we keep just the simple name (`Foo` from `Lcom/example/Foo;`).
    pub labels: Arc<Vec<SharedString>>,
    /// What kind of view each leaf opens. Parallel to `bodies` etc.
    pub kinds: Arc<Vec<LeafKind>>,
    /// Icon asset path for each leaf — chosen at load time from
    /// the leaf kind (and, for smali classes, the parsed
    /// `SmaliClass.source` extension). One of the names
    /// registered by the `IconAssets` source — see `icons.rs`.
    pub leaf_icons: Arc<Vec<&'static str>>,
    /// blake3 of the source bytes — the persistence key. `None` for the
    /// standalone arm64 case until that grows real artifact identity.
    pub bundle_id: Option<glass_db::BundleId>,
    /// Artifact hashes parallel to whatever the snapshot considers an
    /// artifact: each DEX, each native lib. Indices are private to the
    /// snapshot — persistence stores the whole list in the BundleRecord.
    pub artifact_ids: Arc<Vec<glass_db::ArtifactId>>,
    /// Display label for the bundle in the title bar (just the filename).
    pub display_label: String,
    /// Per-native-artifact section info, keyed by ArtifactId.
    /// Empty for DEX-only artifacts.
    pub native_sections: Arc<std::collections::HashMap<glass_db::ArtifactId, Vec<SectionInfo>>>,
    /// Per-native-artifact merged symbol map (symtab + DWARF + .eh_frame).
    pub symbol_maps: Arc<std::collections::HashMap<glass_db::ArtifactId, glass_arch_arm::SymbolMap>>,
    /// Text sections we can disassemble on demand. One entry per
    /// `SectionKind::Text` section per native artifact. Keyed by
    /// `(artifact, section_name)` so the Listing tab can look up by
    /// the same `(artifact, section)` it already carries.
    pub text_sections: Arc<std::collections::HashMap<(glass_db::ArtifactId, String), TextSectionBytes>>,
    /// Non-text section bytes (data / rodata / plt / etc.) for the hex
    /// view. Same `(artifact, section_name)` keying as `text_sections`.
    pub data_sections: Arc<std::collections::HashMap<(glass_db::ArtifactId, String), DataSectionBytes>>,
    /// Smali method-reference → location map. Keyed by the full
    /// `Class;->name(sig)ret` form (as it appears in source), valued
    /// with `(leaf_id, line_index)` — the SmaliClass leaf and the
    /// 0-based line within its body where the `.method` declaration
    /// starts. Built once at load, used by the smali renderer for
    /// method-ref deep links.
    pub method_lines: Arc<std::collections::HashMap<String, (LeafId, usize)>>,
    /// Per-method call index — for each `Class;->name(sig)ret` key,
    /// the deduplicated list of callee keys in first-occurrence
    /// order. Drives the DEX call-graph view.
    pub method_calls: Arc<std::collections::HashMap<String, Vec<String>>>,
    /// Pre-flattened AndroidManifest rows for the XML viewer. Empty
    /// for non-APK bundles or APKs without a parseable manifest.
    pub manifest_rows: Arc<Vec<ManifestRow>>,
    /// Raw parsed AndroidManifest. Kept alongside `manifest_rows`
    /// (which is for the read-only viewer) because the Frida
    /// injection planner walks the typed tree to pick a patch
    /// target. `None` for non-APK bundles and APKs whose manifest
    /// failed to decode.
    pub android_manifest:
        Option<Arc<::smali::android::binary_xml::AndroidManifest>>,
    /// Cross-reference store. Built on background threads after
    /// foreground load completes so first paint stays fast. Right-
    /// click "References / Callers" menus consult this; while a
    /// given index is `Building` the menu shows a progress chip.
    pub xrefs: xref::XrefStore,
    /// Per-artifact annotation index, loaded once at bundle open.
    /// Empty for artifacts with no annotations on disk; the whole
    /// map is empty for bundles that have never had any.
    pub annotations: Arc<std::collections::HashMap<glass_db::ArtifactId, annotations::AnnotationIndex>>,
    /// Staged instruction edits — keyed by (artifact, vaddr). Each
    /// entry replaces a 4-byte instruction with newly-encoded
    /// bytes. In-memory only; closing the bundle drops them.
    /// Listing renderer reads this to recolour + re-disassemble
    /// edited lines on the fly; export walks `entries()` to splice
    /// patched bytes back into the artifact.
    pub edits: edits::EditRegistry,
    /// Original parsed DEX classes, keyed by
    /// `(dex-artifact, class_jni)`. Kept on the snapshot so the
    /// smali editor has a typed value to clone before modifying;
    /// also the source the export path falls back to for unedited
    /// classes when re-emitting a DEX. Cheap shared `Arc`.
    pub smali_classes: Arc<std::collections::HashMap<(glass_db::ArtifactId, String), ::smali::types::SmaliClass>>,
    /// Staged class-level smali edits — keyed by the same
    /// `(artifact, class_jni)` pair as `smali_classes`. In-memory
    /// only; closing the bundle drops them.
    pub smali_edits: smali_edits::SmaliEditRegistry,
    /// Live Frida method-trace registry. Sibling of
    /// `smali_edits` — same `(artifact, class_jni)`-style
    /// keying. Populated by the dock when the user starts a
    /// trace from the smali view; closed when they stop the
    /// trace or disconnect.
    pub traces: traces::TraceRegistry,
    /// Live Frida method-hook registry. Hooks override
    /// method behaviour (return values, args, side effects)
    /// while traces just observe. Same key shape so a
    /// method can carry both at once.
    pub hooks: hooks::HookRegistry,
    /// Extra files to splice into the APK at export time —
    /// keyed by their zip-entry path
    /// (`lib/arm64-v8a/libfrida-gadget.so`, etc.). Used by the
    /// Frida gadget-injection flow to ship the gadget binary
    /// alongside the staged smali edit that loads it. Sorted
    /// (BTreeMap) for deterministic export ordering. In-memory
    /// only; closing the bundle drops them.
    pub pending_additions: std::collections::BTreeMap<String, Vec<u8>>,
}

/// Owned bytes + base address for a text section. Cheap to clone via Arc.
///
/// When the artifact is ARMv7 the loader pre-runs the upstream
/// recursive-descent disassembler against the section's symbol set
/// and stashes the result in `precomputed`. The listing renderer
/// walks that vector instead of decoding 4-byte chunks on demand —
/// Thumb is variable-width and literal pools are interleaved with
/// code, so the fixed-4-byte path used for AArch64 doesn't work.
/// AArch64 leaves `precomputed` as `None` and continues to decode on
/// demand for byte-identical legacy behavior.
#[derive(Clone)]
pub struct TextSectionBytes {
    pub base: u64,
    pub bytes: Arc<Vec<u8>>,
    pub precomputed: Option<Arc<Vec<glass_arch_arm::DecodedInsn>>>,
}

/// Owned bytes + base address for a non-text section, used by the hex
/// view. We could fold this into a single SectionBytes type, but
/// keeping them separate makes the "code vs data" distinction explicit
/// at call sites that only want one or the other.
#[derive(Clone)]
pub struct DataSectionBytes {
    pub base: u64,
    pub bytes: Arc<Vec<u8>>,
    pub kind: NativeSectionKind,
}

impl DataSectionBytes {
    /// How many 16-byte rows the hex view will render.
    pub fn row_count(&self) -> usize {
        self.bytes.len().div_ceil(16)
    }

    /// Base address of the `n`-th row.
    pub fn row_addr(&self, row: usize) -> u64 {
        self.base + (row as u64) * 16
    }

    /// Row that contains `addr`, clamped to range.
    pub fn row_of(&self, addr: u64) -> usize {
        let off = addr.saturating_sub(self.base) as usize;
        (off / 16).min(self.row_count().saturating_sub(1))
    }

    /// Slice of bytes for the given row (1..=16 long).
    pub fn row_bytes(&self, row: usize) -> &[u8] {
        let start = row * 16;
        let end = (start + 16).min(self.bytes.len());
        &self.bytes[start..end]
    }
}

impl TextSectionBytes {
    /// Number of "rows" the listing should render. For AArch64 (and
    /// any section with no precomputed disassembly) this is the
    /// fixed-4-byte instruction count. For ARMv7 it's the length of
    /// the precomputed instruction vector — Thumb/ARM mixed code
    /// can't be addressed by `byte_offset / 4`.
    pub fn instruction_count(&self) -> usize {
        if let Some(p) = &self.precomputed {
            p.len()
        } else {
            self.bytes.len() / 4
        }
    }

    /// Address of the `index`-th row. AArch64: `base + index * 4`.
    /// ARMv7: the address of the `index`-th precomputed instruction
    /// (variable-width).
    pub fn addr_of(&self, index: usize) -> u64 {
        if let Some(p) = &self.precomputed {
            use armv8_encode::mc::InstructionInfo as _;
            p.get(index).map(|i| i.address()).unwrap_or(self.base)
        } else {
            self.base + (index as u64) * 4
        }
    }

    /// Row containing `addr`, clamped to range. ARMv7 binary-searches
    /// the precomputed vector; AArch64 uses fixed-4-byte arithmetic.
    pub fn index_of(&self, addr: u64) -> usize {
        if let Some(p) = &self.precomputed {
            use armv8_encode::mc::InstructionInfo as _;
            // Greatest index with address <= addr.
            let pos = p.binary_search_by(|i| i.address().cmp(&addr));
            match pos {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            }
            .min(p.len().saturating_sub(1))
        } else {
            let off = addr.saturating_sub(self.base) as usize;
            (off / 4).min(self.instruction_count().saturating_sub(1))
        }
    }

    /// `(address, 4-byte chunk, word)` of the `index`-th row.
    /// Returns `None` when the index is past the end OR when the
    /// row is a >4-byte instruction (which can't fit the
    /// `[u8; 4]` slot). Callers that need variable-width support
    /// should use [`Self::precomputed_at`] instead.
    pub fn word_at(&self, index: usize) -> Option<(u64, [u8; 4], u32)> {
        if let Some(p) = &self.precomputed {
            use armv8_encode::mc::InstructionInfo as _;
            let insn = p.get(index)?;
            let raw = insn.size() as usize;
            if raw > 4 {
                return None;
            }
            let off = (insn.address() - self.base) as usize;
            if off + raw > self.bytes.len() {
                return None;
            }
            // Pad short Thumb instructions out to 4 bytes; the high
            // bytes stay zero so consumers that interpret the word
            // as `u32::from_le_bytes` still get a meaningful value
            // for the 16-bit halfword.
            let mut chunk = [0u8; 4];
            chunk[..raw].copy_from_slice(&self.bytes[off..off + raw]);
            Some((insn.address(), chunk, u32::from_le_bytes(chunk)))
        } else {
            let off = index * 4;
            if off + 4 > self.bytes.len() {
                return None;
            }
            let chunk = &self.bytes[off..off + 4];
            let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
            Some((self.addr_of(index), bytes, u32::from_le_bytes(bytes)))
        }
    }

    /// Precomputed instruction at `index`, if any. ARMv7 only —
    /// AArch64 returns `None` so call sites that want the typed
    /// decode fall back to their own `decode_instruction` call.
    pub fn precomputed_at(&self, index: usize) -> Option<&glass_arch_arm::DecodedInsn> {
        self.precomputed.as_ref().and_then(|p| p.get(index))
    }
}




/// Scroll a list so `target_row` sits roughly 10% down the viewport.
/// Leaves room above for the preceding symbol header / last few rows of
/// the previous function. Falls back to ~5 rows of context when the
/// viewport size isn't known yet (first paint).
pub(crate) fn scroll_into_view_with_context(state: &ListState, target_row: usize) {
    let viewport_h = state.viewport_bounds().size.height;
    let row_h = px(LISTING_ROW_HEIGHT);
    let context_rows = if viewport_h > px(0.) {
        let visible = (viewport_h / row_h) as usize;
        (visible / 10).max(3)
    } else {
        5
    };
    let top = target_row.saturating_sub(context_rows);
    state.scroll_to(ListOffset {
        item_ix: top,
        offset_in_item: px(0.),
    });
}

// ---- AndroidManifest XML viewer --------------------------------------------


/// Lightweight, GPU-friendly section descriptor used by the SectionMap
/// view and (later) the SymbolTable / HexDump views.
#[derive(Clone, Debug)]
pub struct SectionInfo {
    pub name: SharedString,
    pub address: u64,
    pub size: u64,
    pub kind: NativeSectionKind,
    /// Convenience: this section's percentage of the artifact's total
    /// section span. Precomputed so the renderer is O(N).
    pub fraction: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSectionKind {
    Text,
    Data,
    Rodata,
    Bss,
    Debug,
    Other,
}

impl NativeSectionKind {
    fn from_armv8(k: armv8_encode::container::SectionKind) -> Self {
        use armv8_encode::container::SectionKind as K;
        match k {
            K::Text => Self::Text,
            K::Data => Self::Data,
            K::Rodata => Self::Rodata,
            K::Bss => Self::Bss,
            K::Debug => Self::Debug,
            K::Other => Self::Other,
        }
    }

    /// IDA-ish palette. Picked so adjacent sections in the strip remain
    /// distinguishable at small widths on a dark background.
    fn colour(self) -> u32 {
        match self {
            Self::Text => 0x4f7cff,   // blue
            Self::Data => 0x4cb964,   // green
            Self::Rodata => 0x4cc8b9, // teal
            Self::Bss => 0x6b6b75,    // grey
            Self::Debug => 0xa57ad6,  // violet
            Self::Other => 0x8a8a92,  // pale grey
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Text => "code",
            Self::Data => "data",
            Self::Rodata => "rodata",
            Self::Bss => "bss",
            Self::Debug => "debug",
            Self::Other => "other",
        }
    }
}

/// Minimal text-only tooltip view. gpui's `tooltip()` API wants an
/// `AnyView`, so we build a tiny entity that just renders its string.
pub struct TextTooltip {
    pub text: SharedString,
}

impl Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg({
                // Tooltip uses a darker variant of the panel background.
                let t = theme::current();
                let p = t.shell.bg.rgba();
                gpui::Rgba { r: p.r * 0.8, g: p.g * 0.8, b: p.b * 0.8, a: 1.0 }
            })
            .border_1()
            .border_color(theme::current().shell.border.rgba())
            .rounded_sm()
            .text_xs()
            .text_color(theme::current().shell.text_bright.rgba())
            .font_family("Menlo")
            .child(self.text.clone())
    }
}

/// What clicking a leaf in the tree should open.
#[derive(Debug, Clone)]
pub enum LeafKind {
    /// Lifted smali for a DEX class. The string is the JNI signature —
    /// stable across DEX reshuffles, so it's also the persistence key.
    SmaliClass { class_jni: String },
    /// AArch64 linear listing over a native artifact's `__text`.
    Listing {
        artifact: glass_db::ArtifactId,
        section: String,
    },
    /// Tabulated hex view of a non-text section.
    Hex {
        artifact: glass_db::ArtifactId,
        section: String,
    },
    /// Section map (overview) for a native artifact.
    SectionMap { artifact: glass_db::ArtifactId },
    /// AndroidManifest.xml viewer.
    Manifest,
    /// Control-flow graph for the function whose entry is `entry_addr`.
    Cfg {
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
    },
    /// DEX method call graph rooted on a specific method.
    DexCallGraph {
        class_jni: String,
        method_decl: String,
    },
}

impl LoadedBundle {
    /// The smali class to render for `(artifact, class_jni)` —
    /// returns the staged edit if any, otherwise the original
    /// parsed class. `None` if the artifact / jni pair isn't a
    /// DEX class known to this bundle.
    ///
    /// Both branches return `&SmaliClass` from differently-owned
    /// containers, so we return a reference into whichever map
    /// has it.
    pub fn smali_class_for(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
    ) -> Option<&::smali::types::SmaliClass> {
        if let Some(edit) = self.smali_edits.get(artifact, class_jni) {
            return Some(&edit.modified);
        }
        let key = (artifact.clone(), class_jni.to_string());
        self.smali_classes.get(&key)
    }

    /// Find the leaf that backs a given persisted tab state. Returns
    /// `None` if the bundle no longer contains it (e.g. a class
    /// disappeared between sessions).
    pub fn resolve(&self, state: &glass_db::TabState) -> Option<LeafId> {
        use glass_db::TabState as TS;
        match state {
            TS::SmaliClass { class_jni, .. } => self.kinds.iter().enumerate().find_map(|(i, k)| {
                match k {
                    LeafKind::SmaliClass { class_jni: this } if this == class_jni => {
                        Some(LeafId(i))
                    }
                    _ => None,
                }
            }),
            TS::Listing { artifact, section, .. } => {
                self.kinds.iter().enumerate().find_map(|(i, k)| match k {
                    LeafKind::Listing { artifact: a, section: s } if a == artifact && s == section => {
                        Some(LeafId(i))
                    }
                    _ => None,
                })
            }
            TS::Hex { artifact, section, .. } => {
                self.kinds.iter().enumerate().find_map(|(i, k)| match k {
                    LeafKind::Hex { artifact: a, section: s } if a == artifact && s == section => {
                        Some(LeafId(i))
                    }
                    _ => None,
                })
            }
            TS::SectionMap { artifact } => {
                self.kinds.iter().enumerate().find_map(|(i, k)| match k {
                    LeafKind::SectionMap { artifact: a } if a == artifact => Some(LeafId(i)),
                    _ => None,
                })
            }
            TS::Manifest => self.kinds.iter().enumerate().find_map(|(i, k)| match k {
                LeafKind::Manifest => Some(LeafId(i)),
                _ => None,
            }),
            _ => None,
        }
    }

    /// Find which section of a native artifact contains `addr`. Only
    /// returns sections we can disassemble (`Text` kind today).
    /// Read the 4 bytes at `addr` in a text section of `artifact`,
    /// honouring any staged edit at that address. Returns None if
    /// the address doesn't fall inside a known text section or is
    /// too close to the section's end to hold a full instruction.
    pub fn bytes_at(
        &self,
        artifact: &glass_db::ArtifactId,
        addr: u64,
    ) -> Option<[u8; 4]> {
        if let Some(edit) = self.edits.get(artifact, addr) {
            if edit.new_bytes.len() == 4 {
                return Some([
                    edit.new_bytes[0],
                    edit.new_bytes[1],
                    edit.new_bytes[2],
                    edit.new_bytes[3],
                ]);
            }
        }
        let section_name = self.text_section_for_addr(artifact, addr)?;
        let key = (artifact.clone(), section_name.to_string());
        let section = self.text_sections.get(&key)?;
        let off = addr.checked_sub(section.base)? as usize;
        if off + 4 > section.bytes.len() {
            return None;
        }
        let bytes: &[u8] = section.bytes.as_ref();
        Some([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
    }

    /// Read the byte at `addr` in any data section of `artifact`,
    /// honouring any staged edit at that address. Returns None if
    /// `addr` doesn't fall inside a known data section.
    pub fn data_byte_at(
        &self,
        artifact: &glass_db::ArtifactId,
        addr: u64,
    ) -> Option<u8> {
        if let Some(edit) = self.edits.covering(artifact, addr) {
            let off = (addr - edit.vaddr) as usize;
            return edit.new_bytes.get(off).copied();
        }
        let section_name = self.data_section_for_addr(artifact, addr)?;
        let key = (artifact.clone(), section_name.to_string());
        let section = self.data_sections.get(&key)?;
        let off = addr.checked_sub(section.base)? as usize;
        section.bytes.as_ref().get(off).copied()
    }

    pub fn text_section_for_addr(
        &self,
        artifact: &glass_db::ArtifactId,
        addr: u64,
    ) -> Option<&str> {
        let sections = self.native_sections.get(artifact)?;
        for sec in sections {
            if sec.kind == NativeSectionKind::Text
                && addr >= sec.address
                && addr < sec.address.saturating_add(sec.size)
            {
                return Some(sec.name.as_ref());
            }
        }
        None
    }

    /// Mirror of `text_section_for_addr` for non-text sections that we
    /// could open in the hex view. BSS is excluded (no on-disk bytes).
    pub fn data_section_for_addr(
        &self,
        artifact: &glass_db::ArtifactId,
        addr: u64,
    ) -> Option<&str> {
        let sections = self.native_sections.get(artifact)?;
        for sec in sections {
            if sec.kind != NativeSectionKind::Text
                && sec.kind != NativeSectionKind::Bss
                && addr >= sec.address
                && addr < sec.address.saturating_add(sec.size)
            {
                return Some(sec.name.as_ref());
            }
        }
        None
    }
}

/// Tree of groups + leaves. Groups can nest arbitrarily (package hierarchy);
/// leaves are the clickable items that have a body.
#[derive(Debug)]
pub struct Tree {
    pub roots: Vec<Node>,
}

#[derive(Debug)]
pub enum Node {
    Group {
        label: SharedString,
        children: Vec<Node>,
    },
    Leaf {
        label: SharedString,
        leaf_id: LeafId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafId(pub usize);

// ---- visible row flattening -------------------------------------------------

#[derive(Clone)]
pub(crate) enum RowKind {
    Group {
        path: Vec<usize>,
        expanded: bool,
        label: SharedString,
    },
    Leaf {
        leaf_id: LeafId,
        label: SharedString,
    },
}

#[derive(Clone)]
pub(crate) struct VisibleRow {
    pub(crate) depth: usize,
    pub(crate) kind: RowKind,
}

pub(crate) fn flatten(tree: &Tree, expanded: &Expanded) -> Vec<VisibleRow> {
    let mut out = Vec::new();
    for (idx, node) in tree.roots.iter().enumerate() {
        walk(node, &mut vec![idx], 0, expanded, &mut out);
    }
    out
}

fn walk(
    node: &Node,
    path: &mut Vec<usize>,
    depth: usize,
    expanded: &Expanded,
    out: &mut Vec<VisibleRow>,
) {
    match node {
        Node::Group { label, children } => {
            let is_open = expanded.contains(path);
            out.push(VisibleRow {
                depth,
                kind: RowKind::Group {
                    path: path.clone(),
                    expanded: is_open,
                    label: label.clone(),
                },
            });
            if is_open {
                for (i, child) in children.iter().enumerate() {
                    path.push(i);
                    walk(child, path, depth + 1, expanded, out);
                    path.pop();
                }
            }
        }
        Node::Leaf { label, leaf_id } => {
            out.push(VisibleRow {
                depth,
                kind: RowKind::Leaf {
                    leaf_id: *leaf_id,
                    label: label.clone(),
                },
            });
        }
    }
}

#[derive(Default, Clone)]
pub(crate) struct Expanded {
    /// Set of node paths that are expanded.
    pub(crate) open: std::collections::HashSet<Vec<usize>>,
}

impl Expanded {
    fn contains(&self, path: &[usize]) -> bool {
        self.open.contains(path)
    }
    fn toggle(&mut self, path: &[usize]) {
        if !self.open.remove(path) {
            self.open.insert(path.to_vec());
        }
    }
}

// ---- view -------------------------------------------------------------------

/// Runtime tab. Mirrors `glass_db::TabState` but holds the live `ListState`
/// for scrolling — that's why it can't itself be serialized.
///
/// Per-tab scroll memory is automatic: each tab owns its own `ListState`,
/// preserving position across tab switches.
/// Process-unique identifier for a tab. Used by background workers
/// (the listing-row builder, etc.) so they can install their results
/// into the tab that requested them — even when two tabs share the
/// same `TabKind` (e.g. two Listing tabs on the same section opened
/// via "Follow in new tab").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TabId(u64);

impl TabId {
    pub(crate) fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

pub(crate) struct Tab {
    /// Unique id minted at construction. Stable across kind/state
    /// mutations but **not** persisted — restored tabs get fresh ids.
    pub(crate) id: TabId,
    /// What this tab represents. Stable across reloads.
    pub(crate) kind: TabKind,
    /// Scroll state for the right pane when this tab is active.
    pub(crate) scroll: ListState,
    /// SmaliClass: cached line split of the body.
    /// Listing: unused (see `listing_rows`).
    pub(crate) lines: Option<Arc<Vec<SharedString>>>,
    /// Listing: precomputed mixed rows.
    pub(crate) listing_rows: Option<Arc<Vec<ListingRow>>>,
    /// While `listing_rows` is being built off-thread, holds the
    /// shared progress structure so the render path can show a bar.
    pub(crate) listing_progress: Option<Arc<Mutex<Progress>>>,
    /// Horizontal scroll offset for the right-pane body.
    pub(crate) h_offset: Pixels,
    /// One-shot scroll target consumed on the next active-tab paint.
    pub(crate) pending_scroll_addr: Option<u64>,
    /// Smali deep-link target — the line index to scroll to once the
    /// tab's smali body is materialised.
    pub(crate) pending_smali_scroll_line: Option<usize>,
    /// Preserved scroll offset captured just before a re-render
    /// that invalidates `tab.lines` (e.g. staging a smali edit).
    /// Consumed by `ensure_active_tab_lines` after the new line
    /// cache is built — restores the exact viewport position so
    /// the user doesn't get yanked back to the top.
    pub(crate) pending_scroll_restore: Option<gpui::ListOffset>,
    /// Index of the currently-selected row in this tab's row list.
    pub(crate) selected_row: Option<usize>,
    /// Hex view: the absolute address of the byte under the user's
    /// cursor, when one is selected.
    pub(crate) selected_byte_addr: Option<u64>,
    /// Hex view: precomputed rows (lazily built).
    pub(crate) hex_rows: Option<Arc<Vec<HexRow>>>,
    /// CFG view state. `Some` only for tabs with `TabKind::Cfg`.
    pub(crate) cfg: Option<CfgViewState>,
    /// DEX call-graph view state.
    pub(crate) dex_callgraph: Option<DexCallGraphState>,
}

/// Per-tab state for a CFG view. Holds the camera (pan + zoom in
/// world units), the lazily-computed `FunctionCfg` for the tab's
/// entry address, and bookkeeping for pan-drag interaction.
#[derive(Clone)]
pub(crate) struct CfgViewState {
    pub(crate) camera: graph::GraphCamera,
    pub(crate) cfg: Option<Arc<glass_arch_arm::FunctionCfg>>,
}

impl CfgViewState {
    pub(crate) fn new(pan_x: f32, pan_y: f32, zoom: f32) -> Self {
        Self {
            camera: graph::GraphCamera::new(pan_x, pan_y, zoom),
            cfg: None,
        }
    }

    pub(crate) fn pan_x(&self) -> f32 {
        self.camera.pan_x
    }
    pub(crate) fn pan_y(&self) -> f32 {
        self.camera.pan_y
    }
    pub(crate) fn zoom(&self) -> f32 {
        self.camera.zoom
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TabKind {
    SmaliClass {
        class_jni: String,
    },
    Listing {
        artifact: glass_db::ArtifactId,
        section: String,
    },
    Hex {
        artifact: glass_db::ArtifactId,
        section: String,
    },
    SectionMap {
        artifact: glass_db::ArtifactId,
    },
    Manifest,
    Cfg {
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
    },
    DexCallGraph {
        class_jni: String,
        method_decl: String,
    },
}

impl TabKind {
    /// Persistable form — round-trips through `glass-db`.
    fn to_state(&self) -> glass_db::TabState {
        match self {
            TabKind::SmaliClass { class_jni } => glass_db::TabState::SmaliClass {
                class_jni: class_jni.clone(),
                scroll_line: 0,
            },
            TabKind::Listing { artifact, section } => glass_db::TabState::Listing {
                artifact: artifact.clone(),
                section: section.clone(),
                scroll_top: 0,
            },
            TabKind::Hex { artifact, section } => glass_db::TabState::Hex {
                artifact: artifact.clone(),
                section: section.clone(),
                scroll_top: 0,
            },
            TabKind::SectionMap { artifact } => glass_db::TabState::SectionMap {
                artifact: artifact.clone(),
            },
            TabKind::Manifest => glass_db::TabState::Manifest,
            TabKind::Cfg { artifact, entry_addr } => glass_db::TabState::Cfg {
                artifact: artifact.clone(),
                entry_addr: *entry_addr,
                // Camera is owned by the Tab's CfgViewState (set at
                // resolve time); 0/0/1 is the open-fresh default.
                pan_x: 0.,
                pan_y: 0.,
                zoom: 1.,
            },
            TabKind::DexCallGraph {
                class_jni,
                method_decl,
            } => glass_db::TabState::DexCallGraph {
                class_jni: class_jni.clone(),
                method_decl: method_decl.clone(),
                pan_x: 0.,
                pan_y: 0.,
                zoom: 1.,
            },
        }
    }

    fn from_kind(kind: &LeafKind) -> Self {
        match kind {
            LeafKind::SmaliClass { class_jni } => TabKind::SmaliClass {
                class_jni: class_jni.clone(),
            },
            LeafKind::Listing { artifact, section } => TabKind::Listing {
                artifact: artifact.clone(),
                section: section.clone(),
            },
            LeafKind::Hex { artifact, section } => TabKind::Hex {
                artifact: artifact.clone(),
                section: section.clone(),
            },
            LeafKind::SectionMap { artifact } => TabKind::SectionMap {
                artifact: artifact.clone(),
            },
            LeafKind::Manifest => TabKind::Manifest,
            LeafKind::Cfg { artifact, entry_addr } => TabKind::Cfg {
                artifact: artifact.clone(),
                entry_addr: *entry_addr,
            },
            LeafKind::DexCallGraph {
                class_jni,
                method_decl,
            } => TabKind::DexCallGraph {
                class_jni: class_jni.clone(),
                method_decl: method_decl.clone(),
            },
        }
    }
}

impl Tab {
    fn new(kind: TabKind) -> Self {
        let cfg = matches!(kind, TabKind::Cfg { .. })
            .then(|| CfgViewState::new(0., 0., 1.));
        let dex_callgraph = matches!(kind, TabKind::DexCallGraph { .. })
            .then(|| DexCallGraphState::new(0., 0., 1.));
        Self {
            id: TabId::next(),
            kind,
            pending_scroll_addr: None,
            pending_smali_scroll_line: None,
            pending_scroll_restore: None,
            scroll: ListState::new(0, ListAlignment::Top, px(2000.)),
            lines: None,
            listing_rows: None,
            listing_progress: None,
            h_offset: px(0.),
            selected_row: None,
            selected_byte_addr: None,
            hex_rows: None,
            cfg,
            dex_callgraph,
        }
    }

    /// Constructor that seeds the camera from persisted state. Used
    /// by the restore path so reopening a CFG tab puts the viewport
    /// back where the user left it.
    fn new_cfg_with_camera(
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
        pan_x: f32,
        pan_y: f32,
        zoom: f32,
    ) -> Self {
        let mut tab = Self::new(TabKind::Cfg { artifact, entry_addr });
        tab.cfg = Some(CfgViewState::new(pan_x, pan_y, zoom));
        tab
    }

    fn new_dex_callgraph_with_camera(
        class_jni: String,
        method_decl: String,
        pan_x: f32,
        pan_y: f32,
        zoom: f32,
    ) -> Self {
        let mut tab = Self::new(TabKind::DexCallGraph { class_jni, method_decl });
        tab.dex_callgraph = Some(DexCallGraphState::new(pan_x, pan_y, zoom));
        tab
    }
}

pub(crate) struct Shell {
    /// Root focus — the bound key combos (cmd-F etc.) and the
    /// palette's on_key_down only fire when this is focused.
    focus_handle: FocusHandle,
    /// Source path the bundle was loaded from. Used so save_state can
    /// remember where to reopen it from (Open Recent).
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) state: ShellState,
    /// Set while loading. UI reads this on every paint to render the bar.
    pub(crate) progress: Option<Arc<Mutex<Progress>>>,
    pub(crate) expanded: Expanded,
    /// Open tabs in display order.
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active_tab: Option<usize>,
    pub(crate) list_state: ListState,
    visible_count: usize,
    /// Most recently measured pixel width of the tab bar container. Written
    /// by a `canvas` prepaint hook each frame so the next render can decide
    /// how many fixed-width tabs fit.
    pub(crate) tab_bar_width: Pixels,
    /// Whether the overflow dropdown is open.
    pub(crate) overflow_open: bool,
    /// Persistence handle. `None` if the DB couldn't be opened — we still
    /// run, just without restore-on-reopen.
    db: Option<glass_db::Database>,
    /// Bounds of the section-map bar in window coordinates, captured by
    /// the canvas hook. Used to translate mouse positions into a section
    /// index for the hover cursor.
    pub(crate) section_bar_bounds: Bounds<Pixels>,
    /// Index of the section the user is hovering on the bar — drives the
    /// vertical cursor line and the row highlight in the table.
    pub(crate) hovered_section: Option<usize>,
    /// Interpolated address under the bar cursor — used to look up the
    /// covering symbol for the tooltip. `None` when the source of hover
    /// is the table (no horizontal position there) or the cursor has
    /// left the bar.
    pub(crate) bar_cursor_addr: Option<u64>,
    /// Window-coordinate x of the bar cursor, for tooltip positioning.
    pub(crate) bar_cursor_x: Option<Pixels>,
    /// Section-map table scroll state — for auto-revealing the hovered row.
    pub(crate) section_table_scroll: ListState,
    section_table_len: usize,
    /// Search index for the current bundle, built lazily on a background
    /// thread the first time the palette is opened.
    pub(crate) search_index: Option<Arc<SearchIndex>>,
    /// Whether the index is currently being built.
    pub(crate) search_indexing: bool,
    /// Palette modal state. Survives close+reopen — the user's last
    /// query and selection come back when they click the icon again.
    pub(crate) palette_open: bool,
    pub(crate) palette_query: crate::text_input::TextInput,
    pub(crate) palette_selected: usize,
    pub(crate) palette_list_state: ListState,
    pub(crate) palette_list_len: usize,
    /// Which mode the palette is in. The two modes share scaffolding
    /// (modal panel, scroll, Enter / Esc) but have separate input
    /// state and result renderers. State is preserved across mode
    /// switches so toggling back doesn't lose what you were typing.
    pub(crate) palette_mode: PaletteMode,
    /// Binary-search mode state — query buffer, last result set,
    /// parse / lookup error (rendered inline under the input row).
    pub(crate) palette_bin_query: crate::text_input::TextInput,
    pub(crate) palette_bin_results: Option<std::sync::Arc<glass_api::BinSearchResult>>,
    /// One `(artifact_id, raw_section_name)` per entry in
    /// `palette_bin_results.matches`. The palette scans every native
    /// artifact globally, so the match's `section` field (which is
    /// labelled `"<alabel> · <section>"` for display) isn't enough
    /// to re-open the right tab — we stash the typed identity
    /// alongside the rendered match here. Empty when there are no
    /// results.
    pub(crate) palette_bin_match_sources: Vec<(glass_db::ArtifactId, String)>,
    pub(crate) palette_bin_error: Option<String>,
    /// Persistent virtualised-list state for the bin-search
    /// results pane. Recreated on every search (with the new
    /// result count) so list internals see a fresh model; kept
    /// across render calls so scrolling actually works.
    pub(crate) palette_bin_list_state: gpui::ListState,
    /// When true, bin-search and insn-search scan only text
    /// sections. Default true — keeps the result set focused on
    /// code so a stray ADRP-shaped data pattern doesn't drown
    /// the real hits.
    pub(crate) palette_bin_code_only: bool,
    /// Which artifact bin-search runs against. Defaults to the
    /// bundle's first artifact at open; can be cycled later when we
    /// grow a dropdown.
    pub(crate) palette_bin_artifact: Option<glass_db::ArtifactId>,
    /// Binary-mode sub-grammar. `Bytes` is the literal byte-mask
    /// language consumed by `bin-search`; `Asm` is the typed-assembly
    /// composer that compiles via `insn-search` before scanning.
    /// Persists across mode switches within the same session.
    pub(crate) palette_bin_grammar: BinaryGrammar,
    /// Index of the currently-highlighted variant in the asm-mode
    /// autocomplete dropdown.
    pub(crate) palette_asm_selected: usize,
    /// Cached candidate list. Rebuilt on every keystroke. Length 0
    /// when the input is empty AND the user hasn't activated asm mode.
    pub(crate) palette_asm_candidates: Vec<glass_api::MatchCandidate>,
    /// When `Some`, the palette is showing a scoped result set
    /// (e.g. "Callers of foo") rather than the bundle-wide search.
    /// Esc clears the scope back to bundle-wide search rather than
    /// closing the palette outright.
    pub(crate) palette_scope: Option<crate::PaletteScope>,
    /// Whether the palette's text input has focus. Set on open and on
    /// any click inside the input area.
    palette_focused: bool,
    /// Right-click context menu state. `None` when no menu is open.
    context_menu: Option<ContextMenuState>,
    /// Whether the About-Glass modal is currently shown.
    pub(crate) about_open: bool,
    /// Whether the right-side annotations pane is visible. Persisted
    /// to the bundle record; default false. Auto-opens on write or
    /// when the user clicks an edge-icon (Phase 4).
    pub(crate) annotations_pane_open: bool,
    /// Active theme for this window. Cloned from the global `ThemeSet`
    /// when the window opens. Re-cloning happens when the user picks a
    /// different theme in settings.
    pub(crate) theme: Arc<theme::Theme>,
    /// Per-bundle window-tint slot (0..=4) — indexes `theme.window_tints`.
    /// Persisted on `BundleRecord`; default 0 (no tint).
    pub(crate) window_tint: u8,
    /// Horizontal scroll offset inside the annotations pane. Same
    /// pattern as the listing's `h_offset` — the row's content area
    /// shifts by -h_offset and a scrollbar at the bottom of the
    /// pane shows position. Not persisted; resets on app restart.
    pub(crate) annotations_pane_h_offset: Pixels,
    /// In-progress annotation edit. `Some` flips the palette into
    /// single-row editor mode (no result list, query == initial
    /// value, Enter commits the write).
    pub(crate) annotation_edit: Option<AnnotationEdit>,
    /// In-progress colour pick. Renders a small swatch popover at
    /// the saved position. `None` when closed.
    pub(crate) colour_picker: Option<ColourPickerState>,
    /// Active instruction-edit. `Some` when the user has double-
    /// clicked a disasm row; the listing renderer swaps the
    /// matching row to a `TextInput`. Enter encodes + stages,
    /// Esc cancels. Only one edit can be in flight at a time.
    pub(crate) disasm_edit: Option<DisasmEditState>,
    /// Active hex-view edit. `Some` when the user has double-
    /// clicked a byte cell or a string item in the hex view.
    /// Mutually exclusive with `disasm_edit` in practice.
    pub(crate) hex_edit: Option<HexEditState>,
    /// Active class-declaration edit. `Some` when the user
    /// double-clicked the `.class` line in a smali tab. Holds the
    /// in-progress form values; nothing commits to the bundle's
    /// `smali_edits` registry until Save fires.
    pub(crate) class_decl_edit: Option<class_decl_popover::ClassDeclEditState>,
    pub(crate) field_edit: Option<field_popover::FieldEditState>,
    pub(crate) method_edit: Option<method_popover::MethodEditState>,
    pub(crate) op_edit: Option<op_editor::OpEditState>,
    pub(crate) annotation_stack: Option<annotation_popover::AnnotationStack>,
    pub(crate) external_edit: Option<external_editor::ExternalEditState>,
    /// Cross-platform device manager — populated once at Shell
    /// construction with both an ADB backend (for Android) and
    /// an iDevice backend (for iOS). `Arc` so the background
    /// poll task can hold a reference without keeping Shell
    /// alive.
    pub(crate) device_manager: std::sync::Arc<glass_device::DeviceManager>,
    /// Latest snapshot from the poll loop. Read at render time
    /// to populate the device picker dropdown; mutated only by
    /// the background task via `cx.update_entity`.
    pub(crate) device_snapshot: Vec<glass_device::DeviceInfo>,
    /// Cached backend status (ADB found / iOS reachable). Read
    /// alongside `device_snapshot` to surface install hints in
    /// the dropdown footer.
    pub(crate) device_backend_status: glass_device::BackendStatus,
    /// The device the user has currently selected. Persists as
    /// long as the device stays in the snapshot; reset to
    /// `None` if it disappears.
    pub(crate) selected_device: Option<glass_device::DeviceId>,
    /// Toolbar chip's dropdown visibility flag.
    pub(crate) device_picker_open: bool,
    /// Cached Frida probe per device, with a coarse TTL. The
    /// poll task refreshes the selected device's entry on a
    /// schedule (frida-server doesn't move). Probes off the
    /// gpui thread because frida-core calls block.
    pub(crate) frida_probes: std::collections::HashMap<
        glass_device::DeviceId,
        FridaProbeCache,
    >,
    /// Active gadget-injection dialog. `Some` from the moment
    /// the user clicks "Inject Frida gadget" in the device
    /// picker until they Cancel or the executor finishes. The
    /// plan is snapshotted at open time so the dialog doesn't
    /// re-run the planner on every render.
    pub(crate) injection_dialog: Option<InjectionDialogState>,
    /// Progress + log lines for an in-flight Inject & Install
    /// pipeline. `Some` while export → sign → adb install is
    /// running; flips to `Some(Done(_))` so the user can read
    /// the final status, then they click Dismiss to clear it.
    pub(crate) injection_progress: Option<InjectionProgress>,
    /// Bottom debug dock — `Some` after the user clicks
    /// Connect on the device picker for a Frida-reachable
    /// device with a loaded APK. Hosts Play / Stop controls
    /// and a small log column. Closing the dock disconnects
    /// the (logical) session — there's no long-lived Frida
    /// state to clean up yet; the dock is per-action today.
    pub(crate) debug_dock: Option<DebugDockState>,
    /// Modal overlay showing the full list of active Frida
    /// traces. Toggled from the dock header. Same shape as
    /// `changes_dialog_open` — a bool, render conditionally
    /// at the root level.
    pub(crate) traces_dialog_open: bool,
    /// Modal listing every active Frida hook. Same shape as
    /// the traces dialog; opened from the dock header.
    pub(crate) hooks_dialog_open: bool,
    /// Key of the hook currently being edited in the dialog
    /// (when the user clicked Edit). `None` when the dialog
    /// is showing the list, `Some` when it's showing the
    /// inline JS editor.
    pub(crate) hook_editor_target: Option<hooks::HookKey>,
    /// Live text the user is typing into the JS editor.
    /// Mirrored from a TextInput entity managed by the
    /// dialog; persists across renders so partial edits
    /// survive blur/refresh.
    pub(crate) hook_editor_buffer: String,
    /// Transient anchor used while the user drags the dock's
    /// top handle. Holds (mouse_y_at_press, dock_height_at_press)
    /// so mouse-move can compute a delta. `None` outside of an
    /// active drag.
    pub(crate) debug_dock_resize_anchor: Option<(Pixels, Pixels)>,
    /// Whether the "N changes" modal dialog is showing.
    pub(crate) changes_dialog_open: bool,
    /// True after the first click of "Abandon all" inside the
    /// changes dialog; the second click actually wipes. Reset on
    /// dialog close so a stale-armed state doesn't carry over.
    pub(crate) changes_dialog_confirm_abandon: bool,
    /// Result of the most recent export attempt. `Ok(path)` =
    /// success, `Err(message)` = failure. Surfaced as a small
    /// status chip in the toolbar until the user dismisses it
    /// or runs another export.
    pub(crate) export_status: Option<Result<std::path::PathBuf, String>>,
    /// True while an export is running (bundle re-open + splice
    /// + write). Drives the progress overlay so the user knows
    /// Glass is working — large APKs can take a couple of
    /// seconds to re-pack.
    pub(crate) export_in_progress: bool,
}

/// Bottom debug-dock state. Captured at Connect time so the
/// dock stays attached to a specific device + package even if
/// the user changes the chip selection.
#[derive(Clone, Debug)]
pub(crate) struct DebugDockState {
    pub device: glass_device::DeviceInfo,
    pub package: String,
    /// Frida agent version captured from the probe at connect
    /// time. Surfaces in the dock header as informational
    /// context.
    pub agent_version: Option<String>,
    /// Append-only log of action outputs. Each entry is a
    /// single line ready to render in the dock's log column.
    pub log: Vec<String>,
    /// Dock height in pixels. Default 180; the user drags the
    /// top edge to resize.
    pub height: Pixels,
    /// Frida session for the gadgeted app. `None` until the
    /// attach completes (or fails); `Some` once it's live.
    /// All script lifecycle goes through this.
    pub session: Option<glass_frida::Session>,
    /// True while we're still attempting the attach. Distinct
    /// from `session = None` so the UI can show a spinner /
    /// disable the Play button before the session is ready.
    pub attaching: bool,
}

/// In-flight Inject & Install pipeline state. Single struct
/// for the whole run; the executor pushes log lines + phase
/// updates onto it through `cx.update_entity`, the dialog
/// reads them at render time.
#[derive(Clone, Debug)]
pub(crate) struct InjectionProgress {
    /// Which step we're currently on. Reads as the headline
    /// in the progress overlay.
    pub phase: InjectionPhase,
    /// Combined stdout / stderr / diagnostic lines from each
    /// step. Rendered as a small log panel under the phase.
    pub log: Vec<String>,
    /// `None` while running, `Some(Ok(path))` once the APK is
    /// on the device (or just on disk when no install), or
    /// `Some(Err(msg))` on any failure.
    pub result: Option<Result<std::path::PathBuf, String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InjectionPhase {
    Exporting,
    Signing,
    Installing,
    Done,
}

impl InjectionPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Exporting => "Exporting patched APK…",
            Self::Signing => "Signing patched APK…",
            Self::Installing => "Installing on device…",
            Self::Done => "Done.",
        }
    }
}

/// State for the gadget-injection dialog. Holds the snapshot
/// of the plan, the target device id, and any in-flight
/// execution status (a stub for now — fills in M3.2c with the
/// real executor).
#[derive(Clone, Debug)]
pub(crate) struct InjectionDialogState {
    pub plan: glass_frida::InjectionPlan,
    /// The device the patched APK is destined for. Persisted
    /// so the dialog can show "Inject & install on Pixel 7"
    /// even after the chip's selection changes. `None` means
    /// the user opened the dialog without a device selected
    /// — Inject is disabled.
    pub target_device: Option<glass_device::DeviceInfo>,
}

/// Per-device cache entry for the Frida probe. The chip reads
/// the most recent successful result; the poll task overwrites
/// it when a fresh probe completes. `pending` lets the chip
/// show "probing…" the first time we look at a newly-selected
/// device.
#[derive(Clone, Debug)]
pub(crate) struct FridaProbeCache {
    pub result: Result<glass_frida::ProbeReport, glass_frida::FridaError>,
    pub probed_at: std::time::Instant,
}

#[derive(Debug, Clone)]
pub(crate) struct DisasmEditState {
    pub artifact: glass_db::ArtifactId,
    pub address: u64,
    /// What the user is typing. Pre-populated with the original
    /// disasm text on entry.
    pub input: crate::text_input::TextInput,
    /// Pretty error from the most recent compile attempt; cleared
    /// on the next keystroke. Rendered as a small chip on the
    /// edit row.
    pub error: Option<String>,
    /// Candidate suggestions for the cursor's current position
    /// — mnemonic templates, symbol names, or registers,
    /// depending on classifier. Refreshed on every keystroke.
    pub suggestions: Vec<EditSuggestion>,
    /// Index of the currently-highlighted suggestion. Up/Down
    /// move; Tab commits it into the input.
    pub suggestion_selected: usize,
}

/// One row in the edit-mode autocomplete dropdown. The label
/// is what the user sees; `commit_text` is what gets spliced
/// into the input when the user accepts (commit_text == label
/// except where we want to insert a decorated form).
#[derive(Debug, Clone)]
pub(crate) struct EditSuggestion {
    pub label: SharedString,
    pub commit_text: String,
    pub detail: SharedString,
    pub kind: EditSuggestionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditSuggestionKind {
    Mnemonic,
    Symbol,
    Register,
}

/// Active hex-view edit. Either a single byte (`length == 1`,
/// the user double-clicked a byte cell) or a multi-byte string
/// run (`length > 1`, the user double-clicked a string item).
#[derive(Debug, Clone)]
pub(crate) struct HexEditState {
    pub artifact: glass_db::ArtifactId,
    pub address: u64,
    pub length: usize,
    pub input: crate::text_input::TextInput,
    pub error: Option<String>,
    pub kind: HexEditKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HexEditKind {
    /// User is editing a single byte's hex pair.
    Byte,
    /// User is editing a NUL-terminated string item; `length`
    /// is the original item length including the trailing NUL.
    String,
}

/// Active inline edit driven by the palette input. Set by an
/// `EditRename` / `EditComment` context-menu activation and
/// cleared on commit or Esc.
#[derive(Clone, Debug)]
pub(crate) struct AnnotationEdit {
    pub artifact: glass_db::ArtifactId,
    pub key: glass_db::AnnotationKey,
    pub facet: AnnotationFacet,
    /// Cached label for the palette chip ("Rename foo" / "Comment
    /// on 0x…").
    pub chip_label: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnnotationFacet {
    Rename,
    Comment,
}

/// Which mode the palette is operating in. Each mode keeps its
/// own input + result state so toggling back doesn't lose work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum PaletteMode {
    /// Bundle-wide fuzzy text search: symbols, classes, fields,
    /// methods, strings. Live-filtered as the user types.
    #[default]
    Text,
    /// Byte-level pattern search (`bin-search`). Pattern compiles
    /// + scans on Enter; results show as a table.
    Binary,
}

/// Sub-grammar within Binary mode. Shares result rendering and
/// artifact selection with Bytes; differs only in how the input
/// query is parsed before scanning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum BinaryGrammar {
    /// Literal byte-mask pattern (`c0 03 5f d6`, `e? ?? ff *`).
    #[default]
    Bytes,
    /// Typed assembly (`mov w0, #1 ; ret`) compiled via
    /// `glass_api::compile_insn_pattern`. Drives an autocomplete
    /// dropdown of matching variants.
    Asm,
}

#[derive(Clone, Debug)]
pub(crate) struct ColourPickerState {
    pub artifact: glass_db::ArtifactId,
    pub key: glass_db::AnnotationKey,
    pub position: gpui::Point<gpui::Pixels>,
    pub current: Option<u32>,
}


impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Publish the active theme so leaf renderers can reach it without
        // every render fn taking a `&Theme` parameter.
        theme::set_active(&self.theme);
        // Theme-derived shell colours. `bg` includes the per-bundle
        // window tint so each window stands out from the others.
        let bg = self.theme.window_bg(self.window_tint);
        let panel = self.theme.shell.panel.rgba();
        let border = self.theme.shell.border.rgba();
        let fg = self.theme.shell.text.rgba();
        let dim = self.theme.shell.text_dim.rgba();
        let accent = self.theme.shell.accent.rgba();
        let hover_bg = self.theme.hovers.standard.rgba();

        let header_text: String = match &self.state {
            ShellState::Ready(b) => b.title.clone(),
            ShellState::Loading => self
                .progress
                .as_ref()
                .and_then(|p| p.lock().ok().map(|p| format!("Glass — Loading {}", p.label)))
                .unwrap_or_else(|| "Glass — Loading…".to_string()),
            ShellState::Error(_) => "Glass — load failed".to_string(),
            ShellState::Empty => "Glass — no bundle loaded".to_string(),
        };
        // Push the same string to the OS window title so the Window
        // menu (and Dock tooltip, and Mission Control card) reads
        // "Glass — …" rather than the binary's lower-case executable
        // name. set_window_title is cheap and idempotent at the
        // platform level, so calling it every render is fine.
        window.set_window_title(&header_text);

        let header = div()
            .h(px(28.))
            .flex_shrink_0()
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .bg(panel)
            .text_sm()
            .text_color(dim)
            .child(div().flex_1().child(header_text))
            // Search affordance — clicking is equivalent to ⌘F.
            .child(
                div()
                    .id("palette-icon")
                    .px_3()
                    .h(px(24.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .text_sm()
                    .text_color(fg)
                    .border_1()
                    .border_color(border)
                    .hover(|s| s.bg(hover_bg))
                    .cursor_pointer()
                    .child("Search")
                    .child(
                        div()
                            .text_xs()
                            .text_color(dim)
                            .child("⌘F"),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _ev, window, cx| {
                            this.toggle_palette(window, cx);
                        }),
                    ),
            );
        // Annotations toggle — same chip style as Search. Renders
        // a small dot in the brand colour when the pane is open so
        // the on/off state reads at a glance.
        let header = if matches!(self.state, ShellState::Ready(_)) {
            let pane_open = self.annotations_pane_open;
            header.child(
                div()
                    .id("annotations-icon")
                    .px_3()
                    .h(px(24.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .text_sm()
                    .text_color(fg)
                    .border_1()
                    .border_color(border)
                    .hover(|s| s.bg(rgb(0x36363c)))
                    .cursor_pointer()
                    .child("Annotations")
                    .child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .rounded_full()
                            .bg(if pane_open { accent } else { hover_bg }),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _ev, _w, cx| {
                            if this.annotations_pane_open {
                                this.close_annotations_pane(cx);
                            } else {
                                this.open_annotations_pane(cx);
                            }
                        }),
                    ),
            )
        } else {
            header
        };
        // Device picker chip — sits to the left of the Edit File
        // affordance so it's always present, regardless of which
        // tab is active. Driven by the background poll task in
        // app.rs::spawn_device_poll.
        let header = header.child(device_picker::render_chip(self, fg, dim, cx));

        // "Edit File" affordance — either the launch button (when
        // no session is active and a smali tab is in front) or the
        // live-watching chip (when a session is running). At most
        // one is on screen at a time.
        let header = if let Some(state) = self.external_edit.as_ref() {
            header.child(external_editor::render_chip(state, fg, dim, cx))
        } else if external_editor::can_open_editor(self) {
            header.child(
                div()
                    .id("edit-file-btn")
                    .px_3()
                    .h(px(24.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .text_sm()
                    .text_color(fg)
                    .border_1()
                    .border_color(self.theme.shell.border.rgba())
                    .hover(|s| s.bg(self.theme.hovers.standard.rgba()))
                    .cursor_pointer()
                    .child(SharedString::from("Edit File"))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _ev, _w, cx| {
                            this.open_active_smali_in_external_editor(cx);
                        }),
                    ),
            )
        } else {
            header
        };

        // Staged-edits chip. Only renders when the loaded bundle
        // has at least one staged edit; clicking opens the
        // Changes dialog (same as ⌘E).
        let edit_count = self
            .bundle()
            .map(|b| b.edits.len() + b.smali_edits.len())
            .unwrap_or(0);
        let header = if edit_count > 0 {
            header.child(
                div()
                    .id("changes-icon")
                    .px_3()
                    .h(px(24.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .text_sm()
                    .text_color(fg)
                    .border_1()
                    .border_color(self.theme.state.committed_change.rgba())
                    .bg(self.theme.state.committed_bg.rgba())
                    .hover(|s| s.bg(self.theme.state.committed_hover.rgba()))
                    .cursor_pointer()
                    .child(SharedString::from(format!("{edit_count} change{}", if edit_count == 1 { "" } else { "s" })))
                    .child(
                        div()
                            .text_xs()
                            .text_color(dim)
                            .child("⌘E"),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _ev, _w, cx| {
                            this.open_changes_dialog(cx);
                        }),
                    ),
            )
        } else {
            header
        };

        // Window-tint swatches. Five small dots; clicking sets
        // `self.window_tint`, which selects which entry of the
        // active theme's `window_tints` array tints the window
        // background. Slot 0 is the neutral baseline. Persisted on
        // the BundleRecord, so each window remembers its tint.
        let current_slot = self.window_tint;
        let theme_for_swatch = self.theme.clone();
        let header = {
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2();
            for slot in 0u8..5 {
                // The on-window tints are near-black and unreadable at
                // 14px. Use a punched-up preview instead so the user
                // can tell the slots apart in the picker.
                let tint = theme_for_swatch.swatch_preview(slot);
                let is_sel = slot == current_slot;
                let border_color = if is_sel { accent } else { border };
                row = row.child(
                    div()
                        .id(("window-tint-swatch", slot as usize))
                        .w(px(14.))
                        .h(px(14.))
                        .rounded_full()
                        .bg(tint)
                        .border_1()
                        .border_color(border_color)
                        .cursor_pointer()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _ev, _w, cx| {
                                this.set_window_tint(slot, cx);
                            }),
                        ),
                );
            }
            header.child(row)
        };

        let body = match &self.state {
            ShellState::Ready(bundle) => {
                let bundle = bundle.clone();
                self.render_two_pane(bundle, cx, panel, border, fg, dim, accent)
                    .into_any_element()
            }
            ShellState::Loading => self
                .render_loading(panel, border, fg, dim, accent)
                .into_any_element(),
            ShellState::Error(msg) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(self.theme.errors.highlight.rgba())
                .child(format!("Load failed: {msg}"))
                .into_any_element(),
            ShellState::Empty => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(dim)
                .child("pass an .apk to `glass gui <path>`")
                .into_any_element(),
        };

        let palette_overlay: Option<gpui::AnyElement> = if self.palette_open {
            Some(
                self.render_palette(panel, border, fg, dim, accent, cx)
                    .into_any_element(),
            )
        } else {
            None
        };

        let context_menu_overlay: Option<gpui::AnyElement> =
            self.context_menu.as_ref().map(|menu| {
                self.render_context_menu(menu, panel, border, fg, accent, cx)
                    .into_any_element()
            });

        let colour_picker_overlay: Option<gpui::AnyElement> =
            self.colour_picker.as_ref().map(|state| {
                colour_picker::render_colour_picker(state, panel, border, fg, cx)
                    .into_any_element()
            });

        let changes_overlay: Option<gpui::AnyElement> = if self.changes_dialog_open {
            Some(changes_dialog::render_changes_dialog(
                self, panel, border, fg, dim, accent, cx,
            ))
        } else {
            None
        };

        let injection_overlay: Option<gpui::AnyElement> = self
            .injection_dialog
            .as_ref()
            .map(|state| {
                injection_dialog::render_injection_dialog(
                    self, state, panel, border, fg, dim, accent, cx,
                )
            });

        let injection_progress_overlay: Option<gpui::AnyElement> = self
            .injection_progress
            .as_ref()
            .map(|progress| {
                injection_dialog::render_injection_progress(
                    progress, panel, border, fg, dim, accent, cx,
                )
            });

        let export_progress_overlay: Option<gpui::AnyElement> = if self.export_in_progress {
            Some(render_export_progress(panel, border, fg, dim))
        } else {
            None
        };

        let string_edit_overlay: Option<gpui::AnyElement> = self
            .hex_edit
            .as_ref()
            .filter(|e| e.kind == HexEditKind::String)
            .map(|state| {
                string_edit_popover::render(state, panel, border, fg, dim, cx)
            });

        let class_decl_overlay: Option<gpui::AnyElement> = self
            .class_decl_edit
            .as_ref()
            .map(|state| {
                let annotations = self
                    .class_annotation_summaries(&state.artifact, &state.class_jni);
                class_decl_popover::render(
                    state, &annotations, panel, border, fg, dim, accent, cx,
                )
            });

        let field_edit_overlay: Option<gpui::AnyElement> = self
            .field_edit
            .as_ref()
            .map(|state| {
                let annotations = self.field_annotation_summaries(
                    &state.artifact,
                    &state.class_jni,
                    &state.original_name,
                    &state.original_signature_jni,
                );
                field_popover::render(
                    state, &annotations, panel, border, fg, dim, accent, cx,
                )
            });

        let method_edit_overlay: Option<gpui::AnyElement> = self
            .method_edit
            .as_ref()
            .map(|state| {
                let annotations = self.method_annotation_summaries(
                    &state.artifact,
                    &state.class_jni,
                    &state.original_name,
                    &state.original_signature_jni,
                );
                method_popover::render(
                    state, &annotations, panel, border, fg, dim, accent, cx,
                )
            });

        let annotation_overlay: Option<gpui::AnyElement> = self
            .annotation_stack
            .as_ref()
            .filter(|s| !s.frames.is_empty())
            .map(|stack| {
                annotation_popover::render(stack, panel, border, fg, dim, accent, cx)
            });


        let disasm_edit_suggestions_overlay: Option<gpui::AnyElement> = self
            .disasm_edit
            .as_ref()
            .filter(|e| !e.suggestions.is_empty())
            .map(|state| render_disasm_edit_suggestions(state, panel, border, dim, cx));

        let op_edit_suggestions_overlay: Option<gpui::AnyElement> = self
            .op_edit
            .as_ref()
            .filter(|e| !e.suggestions.is_empty())
            .map(|state| {
                op_editor::render_suggestions(state, panel, border, dim, cx)
            });

        let about_overlay: Option<gpui::AnyElement> = if self.about_open {
            Some(about::render_about(panel, border, fg, dim, cx))
        } else {
            None
        };

        let mut root = div()
            .id("glass-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(bg)
            .text_color(fg)
            .font_family("Menlo")
            // Cmd-F toggles. Bound globally so it works whatever pane
            // has focus.
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                this.toggle_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteClose, _w, cx| {
                // Esc cancels an in-flight disasm / hex edit before
                // doing anything else — the inline TextInput visually
                // dominates so it should be the first thing dismissed.
                if this.disasm_edit.is_some() {
                    this.cancel_disasm_edit(cx);
                    return;
                }
                if this.hex_edit.is_some() {
                    this.cancel_hex_edit(cx);
                    return;
                }
                if this
                    .annotation_stack
                    .as_ref()
                    .is_some_and(|s| !s.frames.is_empty())
                {
                    this.cancel_annotation_frame(cx);
                    return;
                }
                if this.class_decl_edit.is_some() {
                    this.cancel_class_decl_edit(cx);
                    return;
                }
                if this.field_edit.is_some() {
                    this.cancel_field_edit(cx);
                    return;
                }
                if this.method_edit.is_some() {
                    this.cancel_method_edit(cx);
                    return;
                }
                if this.op_edit.is_some() {
                    // Progressive cancel: if the dropdown is up,
                    // collapse it first so the user can keep
                    // typing in the editor. A second Esc closes
                    // the editor proper.
                    let has_suggestions = this
                        .op_edit
                        .as_ref()
                        .is_some_and(|e| !e.suggestions.is_empty());
                    if has_suggestions {
                        if let Some(state) = this.op_edit.as_mut() {
                            state.suggestions.clear();
                            state.suggestion_selected = 0;
                            cx.notify();
                        }
                        return;
                    }
                    this.cancel_op_edit(cx);
                    return;
                }
                if this.changes_dialog_open {
                    this.close_changes_dialog(cx);
                    return;
                }
                // Esc closes any annotation-edit cleanly first; the
                // palette-as-editor case is unambiguous because the
                // edit's chip is already showing in place of the
                // normal results list.
                if this.annotation_edit.is_some() {
                    this.cancel_annotation_edit(cx);
                    return;
                }
                // Esc on a scoped palette first clears the scope —
                // back to bundle-wide search. Only a second Esc
                // closes the palette outright.
                if this.palette_open && this.palette_scope.is_some() {
                    this.clear_palette_scope(cx);
                    return;
                }
                this.close_palette(cx);
                this.close_context_menu(cx);
                this.close_about(cx);
                this.close_colour_picker(cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteUp, _w, cx| {
                // Priority order: disasm suggestions → palette →
                // listing-row selection.
                if this.disasm_edit.is_some() {
                    this.move_disasm_suggestion_pub(-1, cx);
                    return;
                }
                // While the op-edit inline editor is open the arrow
                // keys should stay inside its TextInput — without
                // this guard they fall through and shift the row
                // selection underneath the editor.
                if this.op_edit.is_some() {
                    op_editor::handle_named_key(this, "up", cx);
                    return;
                }
                if this.palette_open {
                    this.palette_move(-1, cx);
                    return;
                }
                this.move_listing_selection(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteDown, _w, cx| {
                if this.disasm_edit.is_some() {
                    this.move_disasm_suggestion_pub(1, cx);
                    return;
                }
                if this.op_edit.is_some() {
                    op_editor::handle_named_key(this, "down", cx);
                    return;
                }
                if this.palette_open {
                    this.palette_move(1, cx);
                    return;
                }
                this.move_listing_selection(1, cx);
            }))
            .on_action(cx.listener(|this, _: &ListingPageUp, _w, cx| {
                this.listing_page_scroll(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &ListingPageDown, _w, cx| {
                this.listing_page_scroll(1, cx);
            }))
            .on_action(cx.listener(|this, _: &HexCursorLeft, _w, cx| {
                // Class-decl / field popovers steal Left for caret
                // movement inside the focused input. Other edits /
                // palette also need the key for their own buffers.
                if this
                    .annotation_stack
                    .as_ref()
                    .is_some_and(|s| !s.frames.is_empty())
                {
                    annotation_popover::handle_named_key(this, "left", cx);
                    return;
                }
                if this.class_decl_edit.is_some() {
                    class_decl_popover::handle_named_key(this, "left", cx);
                    return;
                }
                if this.field_edit.is_some() {
                    field_popover::handle_named_key(this, "left", cx);
                    return;
                }
                if this.method_edit.is_some() {
                    method_popover::handle_named_key(this, "left", cx);
                    return;
                }
                if this.op_edit.is_some() {
                    op_editor::handle_named_key(this, "left", cx);
                    return;
                }
                if this.disasm_edit.is_some()
                    || this.hex_edit.is_some()
                    || this.palette_open
                {
                    return;
                }
                this.hex_move_byte(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &HexCursorRight, _w, cx| {
                if this
                    .annotation_stack
                    .as_ref()
                    .is_some_and(|s| !s.frames.is_empty())
                {
                    annotation_popover::handle_named_key(this, "right", cx);
                    return;
                }
                if this.class_decl_edit.is_some() {
                    class_decl_popover::handle_named_key(this, "right", cx);
                    return;
                }
                if this.field_edit.is_some() {
                    field_popover::handle_named_key(this, "right", cx);
                    return;
                }
                if this.method_edit.is_some() {
                    method_popover::handle_named_key(this, "right", cx);
                    return;
                }
                if this.op_edit.is_some() {
                    op_editor::handle_named_key(this, "right", cx);
                    return;
                }
                if this.disasm_edit.is_some()
                    || this.hex_edit.is_some()
                    || this.palette_open
                {
                    return;
                }
                this.hex_move_byte(1, cx);
            }))
            // ⌘1 / ⌘2 switch palette modes when the palette is
            // open. Only the palette-open guard prevents the
            // chord from firing in other contexts.
            .on_action(cx.listener(|this, _: &PaletteModeText, _w, cx| {
                if this.palette_open {
                    this.palette_set_mode_text(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PaletteModeBinary, _w, cx| {
                if this.palette_open {
                    this.palette_set_mode_binary(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleChangesDialog, _w, cx| {
                this.toggle_changes_dialog(cx);
            }))
            .on_action(cx.listener(|this, _: &ClassDeclCommit, _w, cx| {
                this.commit_class_decl_edit(cx);
            }))
            .on_action(cx.listener(|this, _: &ClassDeclCancel, _w, cx| {
                this.cancel_class_decl_edit(cx);
            }))
            .on_action(cx.listener(|this, _: &FieldCommit, _w, cx| {
                this.commit_field_edit(cx);
            }))
            .on_action(cx.listener(|this, _: &FieldCancel, _w, cx| {
                this.cancel_field_edit(cx);
            }))
            .on_action(cx.listener(|this, _: &MethodCommit, _w, cx| {
                this.commit_method_edit(cx);
            }))
            .on_action(cx.listener(|this, _: &MethodCancel, _w, cx| {
                this.cancel_method_edit(cx);
            }))
            // Enter activates the palette when it's open. Bound
            // globally because the action keymap consumes Enter
            // before our on_key_down listener has a chance to see it.
            .on_action(cx.listener(|this, _: &PaletteActivate, _w, cx| {
                // Priority order:
                //   1. Disasm suggestion highlight (insert it)
                //   2. Disasm edit in flight (commit it)
                //   3. Hex edit in flight (commit it)
                //   4. Palette open (activate selection)
                //   5. Listing row selected (open it for editing)
                if let Some(e) = this.disasm_edit.as_ref() {
                    if !e.suggestions.is_empty() {
                        this.commit_disasm_suggestion_pub(cx);
                        return;
                    }
                    this.commit_disasm_edit(cx);
                    return;
                }
                if this.hex_edit.is_some() {
                    this.commit_hex_edit(cx);
                    return;
                }
                // The inline op-edit's Enter has to win against the
                // listing-row "edit selected line" handler below.
                // Without this, the action keymap routes Enter
                // straight past us into `edit_selected_listing_row`
                // and the op-edit input never sees it. The action
                // binding is plain `enter` only (no modifiers), so
                // Cmd-Enter still flows through the on_key_down
                // listener and gets the "commit + insert below"
                // treatment there.
                if this.op_edit.is_some() {
                    // Enter inside the op editor: if the
                    // autocomplete dropdown is open, accept the
                    // highlighted suggestion. Only commit the
                    // edit when there's nothing to accept —
                    // otherwise pressing Enter to pick e.g. a
                    // register would also try to parse the
                    // half-finished line.
                    let has_suggestions = this
                        .op_edit
                        .as_ref()
                        .is_some_and(|e| !e.suggestions.is_empty());
                    if has_suggestions {
                        this.accept_op_edit_suggestion(cx);
                    } else {
                        this.commit_op_edit(cx);
                    }
                    return;
                }
                if this.palette_open {
                    this.palette_activate(cx);
                    return;
                }
                if this.hex_open_edit_at_selection(cx) {
                    return;
                }
                // If the selected row in the active smali tab is a
                // class-declaration line, Enter opens the class-decl
                // popover — mirrors the double-click behaviour.
                if this.smali_open_class_decl_at_selection(cx) {
                    return;
                }
                if this.smali_open_field_at_selection(cx) {
                    return;
                }
                if this.smali_open_method_at_selection(cx) {
                    return;
                }
                if this.smali_open_op_edit_at_selection(cx) {
                    return;
                }
                this.edit_selected_listing_row(cx);
            }))
            // Capture printable keystrokes for the palette query when
            // it's open. gpui doesn't have a turnkey text input for
            // arbitrary unicode in this revision — this is enough
            // for the palette.
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _w, cx| {
                let k = &ev.keystroke;
                // Escape always closes the About modal first if it's
                // up — beats palette handling.
                if this.about_open && k.key == "escape" {
                    this.close_about(cx);
                    return;
                }
                // Disasm + hex edits capture all printable keystrokes
                // while active. Enter/Esc are handled by their own
                // actions above (PaletteActivate / PaletteClose) so
                // they don't fall through here.
                if this.disasm_edit.is_some() {
                    this.disasm_edit_handle_key(k, cx);
                    return;
                }
                if this.hex_edit.is_some() {
                    this.hex_edit_handle_key(k, cx);
                    return;
                }
                // Annotation stack sits on top of any parent
                // popover, so it grabs keys first when active.
                if this
                    .annotation_stack
                    .as_ref()
                    .is_some_and(|s| !s.frames.is_empty())
                {
                    annotation_popover::handle_key(this, k, cx);
                    return;
                }
                if this.class_decl_edit.is_some() {
                    class_decl_popover::handle_key(this, k, cx);
                    return;
                }
                if this.field_edit.is_some() {
                    field_popover::handle_key(this, k, cx);
                    return;
                }
                if this.method_edit.is_some() {
                    method_popover::handle_key(this, k, cx);
                    return;
                }
                if this.op_edit.is_some() {
                    op_editor::handle_key(this, k, cx);
                    return;
                }
                if !this.palette_open {
                    return;
                }
                // Palette-global chords that beat the editor:
                //   Tab in Binary+Asm → commit autocomplete template
                //   ⌘B in Binary → toggle bytes / asm grammar
                // Everything else (typing, ⌘V, arrows, etc.) is
                // forwarded to the active TextInput.
                if k.key == "tab"
                    && this.palette_mode == PaletteMode::Binary
                    && this.palette_bin_grammar == BinaryGrammar::Asm
                    && this.annotation_edit.is_none()
                {
                    this.palette_asm_commit_template(cx);
                    return;
                }
                if k.modifiers.platform && k.key == "b"
                    && this.palette_mode == PaletteMode::Binary
                    && this.annotation_edit.is_none()
                {
                    this.palette_toggle_bin_grammar(cx);
                    return;
                }
                this.palette_handle_key(k, cx);
            }))
            .child(header)
            .child(body);
        // Snapshot once and share between the dock + dialog.
        let traces: Vec<traces::TraceEntry> = self
            .bundle()
            .map(|b| b.traces.entries().iter().map(|&e| e.clone()).collect())
            .unwrap_or_default();
        if let Some(dock_state) = self.debug_dock.as_ref() {
            root = root.child(debug_dock::render_debug_dock(
                dock_state, &traces, panel, border, fg, dim, accent, cx,
            ));
        }
        if self.traces_dialog_open {
            root = root.child(traces_dialog::render_traces_dialog(
                &traces, panel, border, fg, dim, accent, cx,
            ));
        }
        if self.hooks_dialog_open {
            let hooks: Vec<hooks::HookEntry> = self
                .bundle()
                .map(|b| b.hooks.entries().iter().map(|&e| e.clone()).collect())
                .unwrap_or_default();
            root = root.child(hooks_dialog::render_hooks_dialog(
                &hooks, panel, border, fg, dim, accent, cx,
            ));
        }
        if let Some(o) = palette_overlay {
            root = root.child(o);
        }
        if let Some(o) = context_menu_overlay {
            root = root.child(o);
        }
        if let Some(o) = colour_picker_overlay {
            root = root.child(o);
        }
        if let Some(o) = about_overlay {
            root = root.child(o);
        }
        if let Some(o) = changes_overlay {
            root = root.child(o);
        }
        if let Some(o) = injection_overlay {
            root = root.child(o);
        }
        if let Some(o) = injection_progress_overlay {
            root = root.child(o);
        }
        if let Some(o) = export_progress_overlay {
            root = root.child(o);
        }
        if let Some(o) = string_edit_overlay {
            root = root.child(o);
        }
        if let Some(o) = class_decl_overlay {
            root = root.child(o);
        }
        if let Some(o) = field_edit_overlay {
            root = root.child(o);
        }
        if let Some(o) = method_edit_overlay {
            root = root.child(o);
        }
        if let Some(o) = annotation_overlay {
            root = root.child(o);
        }
        if let Some(o) = disasm_edit_suggestions_overlay {
            root = root.child(o);
        }
        if let Some(o) = op_edit_suggestions_overlay {
            root = root.child(o);
        }
        if self.device_picker_open {
            root = root.child(device_picker::render_dropdown(
                self, panel, border, fg, dim, accent, cx,
            ));
        }
        root
    }
}


#[derive(Clone)]
pub(crate) enum RowAction {
    Toggle(Vec<usize>),
    Select(LeafId),
}


/// Centred modal saying "Saving patched bundle…". Renders while
/// `Shell::export_in_progress` is true; the click backdrop is
/// inert (no cancel — the re-pack runs uninterruptibly on the
/// background executor). The bar is indeterminate: a shorter
/// coloured chunk slides left↔right inside a track on a 1.4 s
/// loop driven by wall-clock time. Shell's animation pump
/// re-renders at ~30 fps so the position updates smoothly.
fn render_export_progress(
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
) -> gpui::AnyElement {
    use gpui::prelude::*;
    const TRACK_WIDTH: f32 = 320.;
    const CHUNK_WIDTH: f32 = 96.;
    const PERIOD_MS: f32 = 1400.;
    // Wall-clock-driven oscillator in [0, 1] with triangle wave
    // so the chunk pauses briefly at each end.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f32)
        .unwrap_or(0.);
    let t = (now_ms % PERIOD_MS) / PERIOD_MS; // 0..1
    let triangle = if t < 0.5 { t * 2. } else { (1. - t) * 2. }; // 0..1..0
    let chunk_x = triangle * (TRACK_WIDTH - CHUNK_WIDTH);
    let card = gpui::div()
        .id("export-progress-card")
        .w(gpui::px(360.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_md()
        .shadow_lg()
        .p_5()
        .flex()
        .flex_col()
        .gap_3()
        .occlude()
        .child(
            gpui::div()
                .text_lg()
                .text_color(fg)
                .child(gpui::SharedString::from("Saving patched bundle…")),
        )
        .child(
            gpui::div()
                .text_xs()
                .text_color(dim)
                .child(gpui::SharedString::from(
                    "Re-serialising native artifacts and re-packing the archive.",
                )),
        )
        .child(
            // Track + sliding chunk. The track is full-width and
            // the chunk is positioned absolutely inside it; the
            // outer wrapper supplies a relative anchor.
            gpui::div()
                .relative()
                .w(gpui::px(TRACK_WIDTH))
                .h(gpui::px(6.))
                .rounded_sm()
                .bg(theme::current().shell.panel_alt.rgba())
                .child(
                    gpui::div()
                        .absolute()
                        .left(gpui::px(chunk_x))
                        .top(gpui::px(0.))
                        .w(gpui::px(CHUNK_WIDTH))
                        .h(gpui::px(6.))
                        .rounded_sm()
                        .bg(theme::current().shell.accent.rgba()),
                ),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut gpui::App| {
                cx.stop_propagation();
            },
        );
    gpui::div()
        .absolute()
        .inset_0()
        .bg(theme::current().modals.overlay_dark.rgba())
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .child(card)
        .into_any_element()
}

/// Suggestions panel for the active disasm edit. Renders at
/// top-right of the window (out of the way of the listing
/// itself). Up/Down navigates, Tab commits the highlight; key
/// handling lives in Shell::disasm_edit_handle_key.
fn render_disasm_edit_suggestions(
    state: &DisasmEditState,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    use gpui::prelude::*;
    let selected = state.suggestion_selected;
    let mut list = gpui::div()
        .id("disasm-edit-suggestions")
        .w(px(420.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_sm()
        .shadow_lg()
        .flex()
        .flex_col()
        .occlude()
        // Eat clicks on the panel itself so the parent overlay
        // doesn't intercept; per-row clickers run on top.
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut gpui::App| {
                cx.stop_propagation();
            },
        );
    let header_text = match state.suggestions.first().map(|s| s.kind) {
        Some(EditSuggestionKind::Mnemonic) => "Mnemonics  (Tab to insert)",
        Some(EditSuggestionKind::Symbol) => "Symbols  (Tab to insert)",
        Some(EditSuggestionKind::Register) => "Registers  (Tab to insert)",
        None => "Suggestions",
    };
    list = list.child(
        gpui::div()
            .px_2()
            .py_1()
            .text_xs()
            .text_color(dim)
            .border_b_1()
            .border_color(border)
            .child(SharedString::from(header_text)),
    );
    for (i, sugg) in state.suggestions.iter().enumerate().take(12) {
        let is_sel = i == selected;
        let t = theme::current();
        let bg = if is_sel {
            t.modals.palette_selected.rgba()
        } else {
            gpui::rgba(0x00000000)
        };
        let label_color = if is_sel {
            t.shell.text_bright.rgba()
        } else {
            t.shell.text.rgba()
        };
        list = list.child(
            gpui::div()
                .id(("suggestion-row", i))
                .px_2()
                .py_1()
                .bg(bg)
                .flex()
                .flex_row()
                .gap_3()
                .cursor_pointer()
                .hover(|s| s.bg(theme::current().modals.palette_hover.rgba()))
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(label_color)
                        .font_family("Courier New")
                        .child(sugg.label.clone()),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme::current().disasm.address.rgba())
                        .child(sugg.detail.clone()),
                )
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _ev, _w, cx| {
                        this.click_disasm_suggestion(i, cx);
                    }),
                ),
        );
    }
    gpui::div()
        .absolute()
        .top(px(72.))
        .right(px(20.))
        .child(list)
        .into_any_element()
}
