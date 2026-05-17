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
use std::time::Duration;

use anyhow::{Context as _, Result};
use glass_arch_arm64::Arm64Binary;
use glass_mobile::{ApkBundle, Bundle, IpaBundle};
use gpui::{
    App, Bounds, Context, FocusHandle, KeyBinding, ListAlignment, ListOffset, ListState, Pixels,
    Render, SharedString, Window, WindowBounds, WindowOptions, actions, div, list, prelude::*,
    px, rgb, size,
};
use gpui_platform::application;

mod about;
mod app;
mod cfg_block;
mod cfg_render;
mod context_menu;
mod dex_callgraph;
mod dex_cg_render;
mod graph;
mod graph_canvas;
mod hex;
mod listing_model;
mod listing_render;
mod loader;
mod manifest;
mod palette;
mod scrollbar;
mod search;
mod section_map;
mod shell_actions;
mod shell_render;
mod smali;
mod two_pane;

pub use app::launch;
use context_menu::{ContextMenuItem, ContextMenuState};
use dex_callgraph::DexCallGraphState;
use loader::load_bundle_blocking;
pub use loader::snapshot_arm64;
pub use search::{build_search_index, SearchEntry, SearchIndex, SearchJump};
use search::jni_to_dotted;

pub use hex::{build_hex_rows, hex_row_for_addr, HexRow};
pub use listing_model::{
    build_listing_rows, listing_row_for_addr, ArrowDirection, ArrowRole, ArrowSegment, ArrowStyle,
    DataPeek, ListingRow, ARROW_MAX_LANES,
};
use listing_render::{
    render_hex_row, render_listing_row_with, RowCtx, HEX_ROW_HEIGHT, HEX_ROW_MIN_WIDTH,
    LISTING_ROW_HEIGHT, LISTING_ROW_MIN_WIDTH,
};
pub use manifest::{flatten_info_plist, flatten_manifest, ManifestRow};
use palette::{
    chunk_colour, COLOUR_ADDR, COLOUR_ADDRESS_OP, COLOUR_BB_SEPARATOR, COLOUR_BYTES,
    COLOUR_COMMENT, COLOUR_DIRECTIVE, COLOUR_IMMEDIATE, COLOUR_LABEL, COLOUR_MNEMONIC,
    COLOUR_MODIFIER, COLOUR_PLAIN, COLOUR_PUNCT, COLOUR_REGISTER, COLOUR_ROW_SELECTED,
    COLOUR_STRING, COLOUR_SYMBOL_HEADER, COLOUR_TYPE, COLOUR_TYPE_EXTERNAL,
};
use scrollbar::{horizontal_scrollbar_offset, list_scrollbar};
use smali::{extract_class_jni, tokenize_smali_line};

actions!(
    glass,
    [
        TogglePalette,
        PaletteClose,
        PaletteUp,
        PaletteDown,
        PaletteActivate,
        OpenFile,
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
    pub symbol_maps: Arc<std::collections::HashMap<glass_db::ArtifactId, glass_arch_arm64::SymbolMap>>,
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
}

/// Owned bytes + base address for a text section. Cheap to clone via Arc.
#[derive(Clone)]
pub struct TextSectionBytes {
    pub base: u64,
    pub bytes: Arc<Vec<u8>>,
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
        (self.bytes.len() + 15) / 16
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
    pub fn instruction_count(&self) -> usize {
        self.bytes.len() / 4
    }

    pub fn addr_of(&self, index: usize) -> u64 {
        self.base + (index as u64) * 4
    }

    pub fn index_of(&self, addr: u64) -> usize {
        let off = addr.saturating_sub(self.base) as usize;
        (off / 4).min(self.instruction_count().saturating_sub(1))
    }

    pub fn word_at(&self, index: usize) -> Option<(u64, [u8; 4], u32)> {
        let off = index * 4;
        if off + 4 > self.bytes.len() {
            return None;
        }
        let chunk = &self.bytes[off..off + 4];
        let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
        Some((self.addr_of(index), bytes, u32::from_le_bytes(bytes)))
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

/// Find the hex row index containing `addr`, or the nearest one below.
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

/// Lighten an opaque 0xRRGGBB by ~25% per channel. Used to give the
/// hovered section in the bar a "this is the one" lift.

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
            .bg(rgb(0x18181c))
            .border_1()
            .border_color(rgb(0x36363c))
            .rounded_sm()
            .text_xs()
            .text_color(rgb(0xf2f2f2))
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
    /// Find the leaf that backs a given persisted tab state. Returns
    /// `None` if the bundle no longer contains it (e.g. a class
    /// disappeared between sessions).
    pub fn resolve(&self, state: &glass_db::TabState) -> Option<LeafId> {
        use glass_db::TabState as TS;
        match state {
            TS::SmaliClass { class_jni } => self.kinds.iter().enumerate().find_map(|(i, k)| {
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
pub(crate) struct Tab {
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
    pub(crate) cfg: Option<Arc<glass_arch_arm64::FunctionCfg>>,
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

/// Pixels per world unit at zoom = 1.0. World coords are normalised
/// against this so a block at `(x, y)` lands at screen pixel
/// `(viewport_centre + (x - pan_x) * world_unit * zoom)`.
///
/// These mirror the canonical values in `graph::WORLD_UNIT` etc. —
/// kept here as aliases so the CFG-specific rendering can refer to
/// them without an explicit module path. PR B's modularisation step
/// will move the CFG renderer into its own module and these
/// aliases go away.
const CFG_WORLD_UNIT: f32 = graph::WORLD_UNIT;
const CFG_MIN_ZOOM: f32 = graph::MIN_ZOOM;
const CFG_MAX_ZOOM: f32 = graph::MAX_ZOOM;
const CFG_ZOOM_STEP: f32 = graph::ZOOM_STEP;

/// LOD threshold — measured in *pixels of a block's on-screen size*
/// (its width at the current zoom). Below `LOD_PILL_MAX`, a block is
/// just a coloured pill with its label; above it, the block shows
/// the symbol header + first instructions + count summary.
const LOD_PILL_MAX: f32 = 50.;

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
            kind,
            pending_scroll_addr: None,
            pending_smali_scroll_line: None,
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
    palette_open: bool,
    pub(crate) palette_query: String,
    pub(crate) palette_selected: usize,
    pub(crate) palette_list_state: ListState,
    pub(crate) palette_list_len: usize,
    /// Whether the palette's text input has focus. Set on open and on
    /// any click inside the input area.
    palette_focused: bool,
    /// Right-click context menu state. `None` when no menu is open.
    context_menu: Option<ContextMenuState>,
    /// Goto-address bar state. `goto_focused` swallows keystrokes
    /// into `goto_query`; Enter parses + navigates, ESC closes.
    pub(crate) goto_focused: bool,
    pub(crate) goto_query: String,
    /// Whether the About-Glass modal is currently shown.
    pub(crate) about_open: bool,
}


impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = rgb(0x1e1e22);
        let panel = rgb(0x26262c);
        let border = rgb(0x36363c);
        let fg = rgb(0xd6d6d6);
        let dim = rgb(0x808088);
        let accent = rgb(0x4f7cff);

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

        // Pre-build the goto-address widget (only when a bundle is
        // loaded). Returns None otherwise so the header skips it.
        let goto_widget = if matches!(self.state, ShellState::Ready(_)) {
            let goto_focused = self.goto_focused;
            let goto_query = self.goto_query.clone();
            let parsed_ok =
                self.goto_parse().is_some() || goto_query.trim().is_empty();
            let border_col = if !parsed_ok {
                rgb(0xff5050)
            } else if goto_focused {
                rgb(0x4f7cff)
            } else {
                border
            };
            let display = if goto_query.is_empty() {
                SharedString::from("Goto 0x…")
            } else if goto_focused {
                SharedString::from(format!("{}|", goto_query))
            } else {
                SharedString::from(goto_query.clone())
            };
            let display_colour = if goto_query.is_empty() { dim } else { fg };
            Some(
                div()
                    .id("goto-bar")
                    .w(px(180.))
                    .h(px(24.))
                    .px_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .rounded_sm()
                    .text_sm()
                    .text_color(display_colour)
                    .font_family("Courier New")
                    .border_1()
                    .border_color(border_col)
                    .cursor_text()
                    .child(display)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _ev, _w, cx| {
                            this.goto_open(cx);
                        }),
                    ),
            )
        } else {
            None
        };

        let mut header = div()
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
            .child(div().flex_1().child(header_text));
        if let Some(w) = goto_widget {
            header = header.child(w);
        }
        let header = header
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
                    .hover(|s| s.bg(rgb(0x36363c)))
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
                .text_color(rgb(0xff8080))
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
                this.close_palette(cx);
                this.close_context_menu(cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteUp, _w, cx| {
                if this.palette_open {
                    this.palette_move(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PaletteDown, _w, cx| {
                if this.palette_open {
                    this.palette_move(1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PaletteActivate, _w, cx| {
                if this.palette_open {
                    this.palette_activate(cx);
                }
            }))
            // Capture printable keystrokes for the palette query when it's
            // open, or for the goto-address bar when it's focused. gpui
            // doesn't have a turnkey text input for arbitrary unicode in
            // this revision — this is enough for our two text fields.
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _w, cx| {
                let k = &ev.keystroke;
                // Escape always closes the About modal first if it's
                // up — beats palette / goto handling.
                if this.about_open && k.key == "escape" {
                    this.close_about(cx);
                    return;
                }
                if this.goto_focused {
                    if k.key == "escape" {
                        this.goto_close(cx);
                        return;
                    }
                    if k.key == "enter" {
                        this.goto_activate(cx);
                        return;
                    }
                    if k.key == "backspace" {
                        this.goto_backspace(cx);
                        return;
                    }
                    if k.modifiers.platform || k.modifiers.control || k.modifiers.alt {
                        return;
                    }
                    let Some(s) = k.key_char.as_deref() else { return };
                    if s.is_empty() {
                        return;
                    }
                    this.goto_type(s, cx);
                    return;
                }
                if !this.palette_open {
                    return;
                }
                if k.key == "backspace" {
                    this.palette_backspace(cx);
                    return;
                }
                if k.modifiers.platform || k.modifiers.control || k.modifiers.alt {
                    return;
                }
                let Some(s) = k.key_char.as_deref() else { return };
                if s.is_empty() {
                    return;
                }
                this.palette_type(s, cx);
            }))
            .child(header)
            .child(body);
        if let Some(o) = palette_overlay {
            root = root.child(o);
        }
        if let Some(o) = context_menu_overlay {
            root = root.child(o);
        }
        if let Some(o) = about_overlay {
            root = root.child(o);
        }
        root
    }
}


#[derive(Clone)]
pub(crate) enum RowAction {
    Toggle(Vec<usize>),
    Select(LeafId),
}

