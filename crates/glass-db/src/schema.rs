//! On-disk records and the user-facing state types they hold.
//!
//! These are deliberately string-keyed (class JNI signatures, section
//! names) rather than runtime indices, so they survive bundle reloads
//! and even shuffles like classes moving between DEX files.

use serde::{Deserialize, Serialize};

use crate::ids::ArtifactId;

/// Bumped whenever the on-disk shape changes in a non-additive way.
/// Records with a higher version than this binary supports are skipped
/// on read; lower versions are dropped (we don't keep migration code
/// in v1 — start fresh after a bump).
/// v2: `Annotation` became a struct of three optionals (rename /
/// comment / colour) instead of an enum, so a single key can carry
/// all three. Old v1 records with the enum shape fail to decode
/// and are silently skipped — see `decode_versioned` in store.rs.
pub const SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleRecord {
    pub schema_version: u32,
    /// Human-readable name (apk filename when last seen). Pure UI.
    pub label: String,
    pub last_opened_unix: u64,
    pub artifacts: Vec<ArtifactId>,
    pub open_tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
    /// Tree expansion state as a list of node paths (per the tree
    /// flatten algorithm in glass-ui). Encoded as opaque ints — if the
    /// tree shape changes between loads, stale paths are dropped on
    /// the restore side and that's fine.
    pub expanded_paths: Vec<Vec<usize>>,
    /// Last filesystem path the bundle was opened from. Used to power
    /// the Open Recent menu. Missing (`None`) for older records that
    /// pre-date this field.
    #[serde(default)]
    pub source_path: Option<String>,
    /// Whether the user had the right-side annotations pane open
    /// last time. Default false on first open; toggled in the UI.
    /// `#[serde(default)]` so old v2 records keep loading.
    #[serde(default)]
    pub annotations_pane_open: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub schema_version: u32,
    pub label: String,
    pub last_opened_unix: u64,
    /// Subtree expansion state — used when a future revision lets you
    /// open an artifact standalone (without its parent bundle).
    pub expanded_paths: Vec<Vec<usize>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BookmarkRecord {
    pub name: String,
    pub key: AnnotationKey,
}

// ---- Tab state --------------------------------------------------------------
//
// Mirrors the runtime `Tab` enum in glass-ui but uses stable string
// identifiers. The conversion between the two lives in glass-ui.

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TabState {
    /// A lifted smali class. `class_jni` is the JNI signature
    /// (e.g. "Lcom/example/Foo;") — stable across DEX shuffles.
    SmaliClass {
        class_jni: String,
    },
    /// Linear AArch64 listing over a section of a native artifact.
    Listing {
        artifact: ArtifactId,
        section: String,
        scroll_top: u64,
    },
    /// Hex view of a section.
    Hex {
        artifact: ArtifactId,
        section: String,
        scroll_top: u64,
    },
    /// Section map (default view for a native lib).
    SectionMap {
        artifact: ArtifactId,
    },
    /// Symbol table, optionally filtered.
    Symbols {
        artifact: ArtifactId,
        filter: SymbolFilter,
    },
    /// Discovered strings within an artifact.
    Strings {
        artifact: ArtifactId,
        scroll_top: u64,
    },
    /// Android manifest viewer (the host APK's, not an artifact).
    Manifest,
    /// Control-flow graph for a function. `entry_addr` is the
    /// function's entry-point virtual address; the runtime resolves
    /// it against the artifact's symbol map.
    Cfg {
        artifact: ArtifactId,
        entry_addr: u64,
        /// World-space pan (in CFG units). Persisted so reopening a
        /// tab restores the viewport.
        pan_x: f32,
        pan_y: f32,
        /// Camera zoom (1.0 = native pixel scale).
        zoom: f32,
    },
    /// DEX method call graph rooted on a specific method. Uses the
    /// JNI signature so it survives DEX reshuffles.
    DexCallGraph {
        /// Class JNI sig, e.g. `Lcom/example/Foo;`.
        class_jni: String,
        /// `name(sig)ret` token (everything after `->`).
        method_decl: String,
        pan_x: f32,
        pan_y: f32,
        zoom: f32,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum SymbolFilter {
    #[default]
    All,
    Exports,
    Imports,
}

// ---- Annotations ------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AnnotationKey {
    /// A raw address within a native artifact.
    Address(u64),
    /// A symbol by name (resolved via the artifact's symtab).
    Symbol(String),
    /// A DEX class by JNI signature.
    Class(String),
    /// A DEX method: (class JNI, method name + descriptor).
    Method(String, String),
}

/// All three facets that can live on one annotation key. The
/// renderer overlays whichever fields are `Some` — a row can carry
/// a rename + comment + colour simultaneously. Writers merge into
/// the existing record rather than replacing it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Annotation {
    /// User-chosen display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename: Option<String>,
    /// Free-form note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// RGBA tag colour, e.g. `0xff0000ff` = opaque red.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<u32>,
}

impl Annotation {
    pub fn is_empty(&self) -> bool {
        self.rename.is_none() && self.comment.is_none() && self.colour.is_none()
    }
}
