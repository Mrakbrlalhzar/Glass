//! Address-ordered treemap of an artifact's functions, with
//! pan + zoom.
//!
//! Each native artifact's text-section symbols are laid out
//! once at tab-open as a mosaic in *world coordinates* — tile
//! area proportional to symbol size, tile order follows `.text`
//! address order so neighbours in the mosaic are neighbours in
//! the binary. Each frame we transform every tile through the
//! tab's camera (pan + zoom) to get its screen rect, cull
//! off-screen tiles, and render the rest.
//!
//! Tile colour states:
//!   * No recording          → neutral grey.
//!   * Recorded, hits == 0   → cold blue.
//!   * Recorded, hits > 0    → blue → yellow → red ramp.
//!
//! Layout is the *strip-treemap* variant of the squarified
//! algorithm: we don't size-sort the input, so spatial order
//! follows address order. The trade-off is slightly worse
//! aspect ratios per tile, which is fine — the win is that
//! the mosaic carries the binary's link-time structure as a
//! free hint at where subsystems sit.
//!
//! Interaction model:
//!   * Scroll wheel  → pan (drag the world under the cursor).
//!   * Mod+wheel     → zoom anchored at cursor.
//!   * Middle drag   → pan.
//!   * Left-drag on background → pan.
//!   * Left-click on tile      → open Listing at that address.
//!   * Shift-left-click on tile → open in new tab.

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, App, Bounds, Context, Pixels, Point, SharedString,
};

use crate::{LoadedBundle, Shell};

/// World-space layout — laid out once at tab open. Camera
/// transforms each tile to screen coords per frame.
///
/// World units are arbitrary; we lay tiles out in a 1000-wide
/// rect with height chosen by the strip-treemap. Camera zoom
/// converts world units to screen pixels.
#[derive(Clone, Debug)]
pub struct WorldTile {
    pub artifact: glass_db::ArtifactId,
    pub section: String,
    pub symbol_addr: u64,
    pub symbol_size: u64,
    pub display_name: SharedString,
    pub wx: f32,
    pub wy: f32,
    pub ww: f32,
    pub wh: f32,
}

/// Pan + zoom state for the coverage view.
///
/// World → screen transform, with the world origin pinned to
/// the viewport centre at pan=(0,0). Pixels per world unit at
/// zoom=1 is `BASE_PX_PER_WORLD`; the user's zoom multiplies
/// that.
#[derive(Clone, Debug)]
pub struct CoverageCamera {
    pub pan_x: f32,
    pub pan_y: f32,
    pub zoom: f32,
    pub viewport_bounds: Bounds<Pixels>,
    /// `Some(start_pos, start_pan_x, start_pan_y)` mid-drag.
    pub drag_start: Option<(Point<Pixels>, f32, f32)>,
    /// True once we've seen a non-zero viewport; first frame
    /// computes a fit-to-view zoom + pan and flips this on.
    pub initialised: bool,
}

impl Default for CoverageCamera {
    fn default() -> Self {
        Self {
            pan_x: 0.,
            pan_y: 0.,
            zoom: 1.,
            viewport_bounds: Bounds::default(),
            drag_start: None,
            initialised: false,
        }
    }
}

/// Pixels per world unit at zoom = 1. World rectangles are
/// laid out at "natural" size in world units; this constant
/// turns that into a sensible-looking default screen size.
/// 1.0 ⇒ 1 world unit = 1 screen pixel; works because we lay
/// out a mosaic at a target world width of around 1000 and a
/// typical viewport is 600-1600 px wide.
const BASE_PX_PER_WORLD: f32 = 1.0;
const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 30.0;
/// Per-event multiplicative zoom factor. Was 1.1 — a single
/// wheel notch felt twice as fast as the rest of the GUI. The
/// CFG view uses the same constant; this one is local because
/// the coverage mosaic spans a much wider zoom range (whole
/// app → single function) and benefits from finer-grained
/// steps. sqrt(1.1) ≈ 1.0488 → two notches now give back the
/// old single-notch step.
const ZOOM_STEP: f32 = 1.0488;

impl CoverageCamera {
    /// World → screen pixels-per-world-unit at the current zoom.
    pub fn unit(&self) -> f32 {
        BASE_PX_PER_WORLD * self.zoom
    }

    pub fn pan_by(&mut self, dx_px: f32, dy_px: f32) {
        let unit = self.unit();
        if unit <= 0. {
            return;
        }
        self.pan_x -= dx_px / unit;
        self.pan_y -= dy_px / unit;
    }

    pub fn zoom_by(&mut self, anchor: Point<Pixels>, delta: f32) {
        let factor = if delta > 0. {
            ZOOM_STEP
        } else if delta < 0. {
            1. / ZOOM_STEP
        } else {
            return;
        };
        let old_zoom = self.zoom;
        let new_zoom = (old_zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (new_zoom - old_zoom).abs() < f32::EPSILON {
            return;
        }
        let bounds = self.viewport_bounds;
        let centre_x = bounds.origin.x.as_f32() + bounds.size.width.as_f32() / 2.;
        let centre_y = bounds.origin.y.as_f32() + bounds.size.height.as_f32() / 2.;
        let old_unit = BASE_PX_PER_WORLD * old_zoom;
        let new_unit = BASE_PX_PER_WORLD * new_zoom;
        let ax = anchor.x.as_f32();
        let ay = anchor.y.as_f32();
        // World point under the cursor stays under the cursor
        // post-zoom: zoom anchored at the cursor.
        let world_x = self.pan_x + (ax - centre_x) / old_unit;
        let world_y = self.pan_y + (ay - centre_y) / old_unit;
        self.zoom = new_zoom;
        self.pan_x = world_x - (ax - centre_x) / new_unit;
        self.pan_y = world_y - (ay - centre_y) / new_unit;
    }

    pub fn drag_start(&mut self, pos: Point<Pixels>) {
        self.drag_start = Some((pos, self.pan_x, self.pan_y));
    }

    pub fn drag_move(&mut self, pos: Point<Pixels>) {
        let Some((start_pos, start_pan_x, start_pan_y)) = self.drag_start else {
            return;
        };
        let unit = self.unit();
        if unit <= 0. {
            return;
        }
        let dx = (pos.x - start_pos.x).as_f32() / unit;
        let dy = (pos.y - start_pos.y).as_f32() / unit;
        self.pan_x = start_pan_x - dx;
        self.pan_y = start_pan_y - dy;
    }

    pub fn drag_end(&mut self) {
        self.drag_start = None;
    }

    /// Fit a `world_w × world_h` rect (origin at world (0,0))
    /// into the current viewport with a small margin. Called
    /// once on first paint after we have real bounds.
    pub fn fit_to(&mut self, world_w: f32, world_h: f32) {
        let bounds = self.viewport_bounds;
        let vw = bounds.size.width.as_f32();
        let vh = bounds.size.height.as_f32();
        if vw <= 0. || vh <= 0. || world_w <= 0. || world_h <= 0. {
            return;
        }
        let margin = 0.95;
        let zoom_x = vw * margin / (world_w * BASE_PX_PER_WORLD);
        let zoom_y = vh * margin / (world_h * BASE_PX_PER_WORLD);
        self.zoom = zoom_x.min(zoom_y).clamp(MIN_ZOOM, MAX_ZOOM);
        // Centre the world rect on the viewport: world centre
        // sits at pan = (world_w/2, world_h/2).
        self.pan_x = world_w / 2.;
        self.pan_y = world_h / 2.;
        self.initialised = true;
    }
}

/// One tile in screen-space after the camera transform. Used
/// by the renderer.
#[derive(Clone, Debug)]
struct ScreenTile {
    artifact: glass_db::ArtifactId,
    section: String,
    symbol_addr: u64,
    display_name: SharedString,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Layout result: world tiles + the world bounding box. The
/// camera uses the bounding box for "fit to view".
#[derive(Clone, Debug)]
pub struct MosaicLayout {
    /// Function tiles for native islands. DEX islands don't
    /// appear here — their content lives in `dex_content`.
    pub tiles: Vec<WorldTile>,
    pub islands: Vec<Island>,
    /// Per-island DEX content. Parallel to `islands` — index N
    /// here corresponds to island N. `None` for native
    /// islands. Defaults populated for all entries so callers
    /// can index without checking length.
    pub dex_content: Vec<Option<DexContent>>,
    pub world_w: f32,
    pub world_h: f32,
    /// The ABI we chose to render for the native islands.
    /// `None` when the bundle has no native code.
    pub chosen_abi: Option<IslandKind>,
    /// Number of native artifacts in the bundle whose ABI we
    /// *didn't* pick. Shown in the header so the user knows
    /// the view is filtered. Does not count DEX artifacts.
    pub hidden_artifact_count: usize,
}

/// One island in the global view — one per native artifact
/// (flat function tiles) or one per DEX artifact (packages
/// containing classes). Contains the artifact's label and its
/// bounding rect in world space.
#[derive(Clone, Debug)]
#[allow(dead_code)] // `artifact` will drive click-to-zoom-into-island
pub struct Island {
    pub artifact: glass_db::ArtifactId,
    pub label: SharedString,
    pub kind: IslandKind,
    pub wx: f32,
    pub wy: f32,
    pub ww: f32,
    pub wh: f32,
    /// Header strip height in world units (scales with the
    /// camera). The header carries the island label and a
    /// kind-derived background tint (native = neutral grey,
    /// DEX = muted purple).
    pub header_h: f32,
}

/// A package rectangle inside a DEX island. Contains class
/// rectangles nested inside it. The package's `wx/wy/ww/wh`
/// are absolute in world space — the renderer doesn't have to
/// chase offsets up the tree.
#[derive(Clone, Debug)]
pub struct PackageRect {
    /// Dotted package name, e.g. `com.example.foo`. Empty
    /// string for classes at the default package.
    pub name: SharedString,
    pub wx: f32,
    pub wy: f32,
    pub ww: f32,
    pub wh: f32,
    pub classes: Vec<ClassRect>,
}

/// A class rectangle inside a package. World coordinates are
/// absolute (same convention as `PackageRect`).
#[derive(Clone, Debug)]
pub struct ClassRect {
    /// JNI form (`Lcom/example/Foo;`) — what `open_smali_editor_for_class`
    /// expects.
    pub class_jni: String,
    /// Just the simple name (`Foo`) for the tile label.
    pub display_name: SharedString,
    pub wx: f32,
    pub wy: f32,
    pub ww: f32,
    pub wh: f32,
}

/// Per-DEX-island package + class tree. Stored as a parallel
/// vec to `MosaicLayout.islands` — keyed by the same index.
/// Native islands have no entry here; their content is in
/// `MosaicLayout.tiles` instead.
#[derive(Clone, Debug, Default)]
pub struct DexContent {
    pub packages: Vec<PackageRect>,
}

/// What kind of code an island contains. Used today for the
/// ABI-filter priority order; once DEX islands land, also
/// for the header tint that distinguishes native vs DEX.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Dex reserved for once we have Java-side coverage
pub enum IslandKind {
    NativeArm64,
    NativeArm,
    NativeX86_64,
    NativeX86,
    NativeOther,
    Dex,
}

impl IslandKind {
    /// From an artifact path label like `lib/arm64-v8a/libfoo.so`.
    pub fn from_label(label: &str) -> Self {
        if label.contains("arm64-v8a") || label.contains("aarch64") {
            Self::NativeArm64
        } else if label.contains("armeabi") || label.contains("/arm/") {
            Self::NativeArm
        } else if label.contains("x86_64") || label.contains("/x86_64/") {
            Self::NativeX86_64
        } else if label.contains("x86") || label.contains("i686") {
            Self::NativeX86
        } else {
            Self::NativeOther
        }
    }

    /// Short label for the header chip — "arm64", "arm", etc.
    pub fn short_label(self) -> &'static str {
        match self {
            Self::NativeArm64 => "arm64",
            Self::NativeArm => "arm",
            Self::NativeX86_64 => "x86_64",
            Self::NativeX86 => "x86",
            Self::NativeOther => "native",
            Self::Dex => "dex",
        }
    }
}

/// Island background tones. Muted enough that tile colours
/// read on top, distinct enough to tell native vs DEX apart
/// at a glance. DEX gets a slight purple tilt.
// Native = warm rusty-brown, DEX = cool forest-green. Both
// muted enough to keep the eventual coverage hot/cold ramp
// readable on top, but far enough apart on the colour wheel
// that the native-vs-DEX distinction is immediate.
const ISLAND_BG_NATIVE: u32 = 0x3a2a26;
const ISLAND_BG_DEX: u32 = 0x263a2a;

fn island_bg(kind: IslandKind) -> u32 {
    match kind {
        IslandKind::Dex => ISLAND_BG_DEX,
        _ => ISLAND_BG_NATIVE,
    }
}

/// Dim-grey border for package outlines drawn over the
/// class tiles. Faint enough that it doesn't fight the class
/// boundaries; visible enough to mark where packages begin.
/// 0xRRGGBBAA — alpha ~22%.
fn rgba_dim_border() -> gpui::Rgba {
    gpui::rgba(0x8c8c8c38)
}

impl MosaicLayout {
    pub fn empty() -> Self {
        Self {
            tiles: Vec::new(),
            islands: Vec::new(),
            dex_content: Vec::new(),
            world_w: 0.,
            world_h: 0.,
            chosen_abi: None,
            hidden_artifact_count: 0,
        }
    }
}

/// Build the global mosaic. Each native artifact (in the
/// chosen ABI) becomes an island with a flat function tile
/// list. Each DEX artifact becomes an island with a nested
/// package → class tree. Outer treemap sizes the islands by
/// their total code volume — native bytes for .so, summed
/// smali op counts for .dex.
///
/// Empty bundle (no native code *and* no DEX) → empty layout.
pub fn build_mosaic_global(bundle: &LoadedBundle) -> MosaicLayout {
    // ---- Native candidates --------------------------------
    let mut all_native: Vec<(
        glass_db::ArtifactId,
        u64,
        SharedString,
        IslandKind,
    )> = Vec::new();
    for (aid, sm) in bundle.symbol_maps.iter() {
        let total_bytes: u64 = sm
            .iter()
            .filter(|s| matches!(s.kind, glass_arch_arm::SymbolKind::Function))
            .filter(|s| s.size > 0)
            .map(|s| s.size)
            .sum();
        if total_bytes == 0 {
            continue;
        }
        let label = bundle
            .native_artifact_labels
            .get(aid)
            .cloned()
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from(aid.to_string()));
        let kind = IslandKind::from_label(label.as_ref());
        all_native.push((aid.clone(), total_bytes, label, kind));
    }

    // Pick one ABI to show among native islands. Priority:
    // arm64 → arm → x86_64 → x86 → other. First match wins
    // even on a partial set; rooted Android is overwhelmingly
    // arm64. "Other" is the catch-all when no ABI label
    // applies (e.g. a standalone .so loaded outside an APK).
    let priority = [
        IslandKind::NativeArm64,
        IslandKind::NativeArm,
        IslandKind::NativeX86_64,
        IslandKind::NativeX86,
        IslandKind::NativeOther,
    ];
    let chosen_abi = priority
        .iter()
        .copied()
        .find(|k| all_native.iter().any(|(_, _, _, candidate_kind)| candidate_kind == k));
    let native_total_count = all_native.len();
    let native_candidates: Vec<(glass_db::ArtifactId, u64, SharedString)> =
        match chosen_abi {
            Some(chosen) => all_native
                .into_iter()
                .filter(|(_, _, _, k)| *k == chosen)
                .map(|(a, b, l, _)| (a, b, l))
                .collect(),
            None => Vec::new(),
        };
    let hidden_artifact_count =
        native_total_count.saturating_sub(native_candidates.len());

    // ---- DEX candidates -----------------------------------
    // Group classes by artifact id; each artifact becomes one
    // DEX island. Skip artifacts whose smali class set is
    // empty (shouldn't normally happen).
    let mut dex_classes_by_artifact: std::collections::HashMap<
        glass_db::ArtifactId,
        Vec<(String, u64)>,
    > = std::collections::HashMap::new();
    for ((aid, class_jni), class) in bundle.smali_classes.iter() {
        let size = smali_class_size(class);
        if size == 0 {
            continue;
        }
        dex_classes_by_artifact
            .entry(aid.clone())
            .or_default()
            .push((class_jni.clone(), size));
    }

    // DEX-artifact size = sum of class sizes. Used by the
    // outer treemap to scale the island vs the native ones.
    let mut dex_candidates: Vec<(glass_db::ArtifactId, u64, SharedString)> =
        Vec::new();
    for (aid, classes) in &dex_classes_by_artifact {
        let total: u64 = classes.iter().map(|(_, s)| s).sum();
        if total == 0 {
            continue;
        }
        // Label: prefer a friendlier "classes.dex" derived
        // from the bundle tree if available; fall back to the
        // artifact id prefix.
        let label = SharedString::from(format!("dex {}…", &aid.to_string()[..8.min(aid.to_string().len())]));
        dex_candidates.push((aid.clone(), total, label));
    }

    if native_candidates.is_empty() && dex_candidates.is_empty() {
        return MosaicLayout::empty();
    }

    // ---- Outer treemap ------------------------------------
    let outer_world_w = 1200.0_f64;
    let outer_aspect = 4.0_f64 / 3.0;
    let outer_target_h = outer_world_w / outer_aspect;
    let outer_area = outer_world_w * outer_target_h;

    // Native bytes and DEX bytes are different units (native
    // .text bytes vs DEX op counts). Mixing them straight gives
    // DEX way too much area. Scale DEX so its total area
    // roughly matches a notional "size" that's comparable. The
    // simplest sane heuristic: scale DEX bytes so the largest
    // DEX island isn't bigger than the largest native island.
    // When there's no native code, DEX uses its raw totals.
    let max_native = native_candidates.iter().map(|(_, b, _)| *b).max().unwrap_or(0);
    let max_dex = dex_candidates.iter().map(|(_, b, _)| *b).max().unwrap_or(0);
    let dex_scale: f64 = if max_native == 0 || max_dex == 0 {
        1.0
    } else {
        (max_native as f64 / max_dex as f64).max(0.0001)
    };

    // Build outer inputs: one row per candidate with the
    // origin kind so we know which content-builder to call.
    enum OuterCandidate {
        Native(glass_db::ArtifactId, SharedString, IslandKind),
        Dex(glass_db::ArtifactId, SharedString),
    }
    let mut all_outer: Vec<(OuterCandidate, f64)> = Vec::new();
    for (a, b, l) in &native_candidates {
        let kind = IslandKind::from_label(l.as_ref());
        all_outer.push((
            OuterCandidate::Native(a.clone(), l.clone(), kind),
            *b as f64,
        ));
    }
    for (a, b, l) in &dex_candidates {
        all_outer.push((
            OuterCandidate::Dex(a.clone(), l.clone()),
            (*b as f64) * dex_scale,
        ));
    }

    let total_units: f64 = all_outer.iter().map(|(_, u)| u).sum();
    if total_units <= 0.0 {
        return MosaicLayout::empty();
    }
    let mut outer_inputs: Vec<(usize, f64)> = all_outer
        .iter()
        .enumerate()
        .map(|(i, (_, u))| (i, *u / total_units * outer_area))
        .collect();
    outer_inputs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let outer_rects = squarified_outer(
        outer_inputs.iter().map(|(_, a)| *a).collect(),
        outer_world_w,
    );
    let mut outer_world_h = 0.0_f64;
    for r in &outer_rects {
        let bottom = r.y + r.h;
        if bottom > outer_world_h {
            outer_world_h = bottom;
        }
    }

    // ---- Inner layout per island --------------------------
    let header_h_world = 24.0_f64;
    let margin_world = 4.0_f64;
    // Visible gap *between* islands. The squarified outer
    // treemap packs rects edge-to-edge; inset each one by
    // gap/2 on every side so neighbours don't share a border.
    let island_gap_world = 12.0_f64;
    let inset = island_gap_world / 2.0;

    let mut all_tiles: Vec<WorldTile> = Vec::new();
    let mut islands: Vec<Island> = Vec::new();
    let mut dex_content: Vec<Option<DexContent>> = Vec::new();
    for (i, (orig_idx, _area)) in outer_inputs.iter().enumerate() {
        let r_raw = &outer_rects[i];
        // Inset the island's bounding rect for the gutter.
        // If a rect is too small to inset without disappearing,
        // skip it entirely — the treemap occasionally produces
        // razor-thin slivers for tiny artifacts.
        if r_raw.w <= island_gap_world || r_raw.h <= island_gap_world {
            continue;
        }
        let isl_x = r_raw.x + inset;
        let isl_y = r_raw.y + inset;
        let isl_w = r_raw.w - island_gap_world;
        let isl_h = r_raw.h - island_gap_world;
        let inner_x = isl_x + margin_world;
        let inner_y = isl_y + header_h_world + margin_world;
        let inner_w = (isl_w - 2.0 * margin_world).max(8.0);
        let inner_h = (isl_h - header_h_world - 2.0 * margin_world).max(8.0);

        match &all_outer[*orig_idx].0 {
            OuterCandidate::Native(aid, label, kind) => {
                let inner_tiles =
                    build_artifact_tiles(bundle, aid, inner_w, inner_h);
                for mut t in inner_tiles {
                    t.wx += inner_x as f32;
                    t.wy += inner_y as f32;
                    all_tiles.push(t);
                }
                islands.push(Island {
                    artifact: aid.clone(),
                    label: label.clone(),
                    kind: *kind,
                    wx: isl_x as f32,
                    wy: isl_y as f32,
                    ww: isl_w as f32,
                    wh: isl_h as f32,
                    header_h: header_h_world as f32,
                });
                dex_content.push(None);
            }
            OuterCandidate::Dex(aid, label) => {
                let classes =
                    dex_classes_by_artifact.get(aid).cloned().unwrap_or_default();
                let packages = build_dex_packages(
                    &classes,
                    inner_x,
                    inner_y,
                    inner_w,
                    inner_h,
                );
                islands.push(Island {
                    artifact: aid.clone(),
                    label: label.clone(),
                    kind: IslandKind::Dex,
                    wx: isl_x as f32,
                    wy: isl_y as f32,
                    ww: isl_w as f32,
                    wh: isl_h as f32,
                    header_h: header_h_world as f32,
                });
                dex_content.push(Some(DexContent { packages }));
            }
        }
    }

    MosaicLayout {
        tiles: all_tiles,
        islands,
        dex_content,
        world_w: outer_world_w as f32,
        world_h: outer_world_h as f32,
        chosen_abi,
        hidden_artifact_count,
    }
}

/// Size proxy for a DEX class — sum of op counts across its
/// methods. Empty / abstract classes get a floor of 1 so they
/// still render as a clickable tile when the user zooms in.
fn smali_class_size(class: &::smali::types::SmaliClass) -> u64 {
    let s: u64 = class.methods.iter().map(|m| m.ops.len() as u64).sum();
    s.max(1)
}

/// Build the per-package and per-class tree inside one DEX
/// island. Returns absolute-world-coordinate `PackageRect`s
/// each containing `ClassRect`s. `inner_x/y/w/h` is the inner
/// area of the island (after header + margin).
fn build_dex_packages(
    classes: &[(String, u64)],
    inner_x: f64,
    inner_y: f64,
    inner_w: f64,
    inner_h: f64,
) -> Vec<PackageRect> {
    if classes.is_empty() || inner_w < 4.0 || inner_h < 4.0 {
        return Vec::new();
    }
    // Group classes by package. Package = everything before
    // the last '/' in the JNI form (`Lcom/example/Foo;` →
    // `com/example`). Classes at the default package go into
    // an empty-string package.
    let mut by_pkg: std::collections::BTreeMap<String, Vec<(String, u64)>> =
        std::collections::BTreeMap::new();
    for (jni, size) in classes {
        let pkg = package_of_jni(jni).to_string();
        by_pkg.entry(pkg).or_default().push((jni.clone(), *size));
    }

    // Outer (package) layout: squarified, sized by total
    // class bytes in the package.
    let pkg_totals: Vec<(String, f64)> = by_pkg
        .iter()
        .map(|(p, cs)| (p.clone(), cs.iter().map(|(_, s)| *s as f64).sum()))
        .collect();
    let total: f64 = pkg_totals.iter().map(|(_, t)| t).sum();
    if total <= 0.0 {
        return Vec::new();
    }
    let inner_area = inner_w * inner_h;
    let mut pkg_inputs: Vec<(usize, f64)> = pkg_totals
        .iter()
        .enumerate()
        .map(|(i, (_, t))| (i, *t / total * inner_area))
        .collect();
    pkg_inputs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let pkg_rects = squarified_in_box(
        pkg_inputs.iter().map(|(_, a)| *a).collect(),
        inner_w,
        inner_h,
    );

    let mut out = Vec::with_capacity(pkg_totals.len());
    for (i, (orig_idx, _area)) in pkg_inputs.iter().enumerate() {
        let pr = &pkg_rects[i];
        let (pkg_name, _total) = &pkg_totals[*orig_idx];
        // Build the class layout inside this package rect.
        // No header strip for packages — the package's name
        // is shown only when the package tile itself is big
        // enough to render at the current zoom (renderer
        // decides). So the entire pkg rect is class area.
        let pkg_classes = by_pkg.get(pkg_name).cloned().unwrap_or_default();
        let class_total: f64 = pkg_classes.iter().map(|(_, s)| *s as f64).sum();
        let class_rects = if class_total > 0.0 && pr.w > 2.0 && pr.h > 2.0 {
            let class_areas: Vec<f64> = pkg_classes
                .iter()
                .map(|(_, s)| (*s as f64) / class_total * pr.w * pr.h)
                .collect();
            // Sort desc so squarified is well-behaved; we
            // don't care about class order within a package.
            let mut indexed: Vec<(usize, f64)> =
                class_areas.iter().copied().enumerate().collect();
            indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let rects = squarified_in_box(
                indexed.iter().map(|(_, a)| *a).collect(),
                pr.w,
                pr.h,
            );
            let mut out_classes = Vec::with_capacity(rects.len());
            for (k, (orig_class_idx, _a)) in indexed.iter().enumerate() {
                let cr = &rects[k];
                let (jni, _size) = &pkg_classes[*orig_class_idx];
                let display = simple_class_name(jni);
                out_classes.push(ClassRect {
                    class_jni: jni.clone(),
                    display_name: SharedString::from(display),
                    wx: (inner_x + pr.x + cr.x) as f32,
                    wy: (inner_y + pr.y + cr.y) as f32,
                    ww: cr.w as f32,
                    wh: cr.h as f32,
                });
            }
            out_classes
        } else {
            Vec::new()
        };

        out.push(PackageRect {
            name: SharedString::from(pkg_name.replace('/', ".")),
            wx: (inner_x + pr.x) as f32,
            wy: (inner_y + pr.y) as f32,
            ww: pr.w as f32,
            wh: pr.h as f32,
            classes: class_rects,
        });
    }
    out
}

/// Package portion of a JNI class string. `Lcom/example/Foo;`
/// → `com/example`. Classes at the default package return
/// the empty string.
fn package_of_jni(jni: &str) -> &str {
    // Strip leading `L` and trailing `;` defensively.
    let inner = jni
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(jni);
    match inner.rfind('/') {
        Some(i) => &inner[..i],
        None => "",
    }
}

/// Simple class name from a JNI string. `Lcom/example/Foo;` →
/// `Foo`. Nested classes (`Foo$Bar`) keep the dollar form.
fn simple_class_name(jni: &str) -> String {
    let inner = jni
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(jni);
    match inner.rfind('/') {
        Some(i) => inner[i + 1..].to_string(),
        None => inner.to_string(),
    }
}

/// Squarified treemap inside an arbitrary-size box. Same
/// algorithm as `squarified_outer` but accepts a custom
/// width *and* height (returns rects that fit inside, with
/// the row-strip axis being width). Inputs are pre-sorted
/// desc; areas should be scaled so they sum to ≤ w*h.
fn squarified_in_box(areas: Vec<f64>, w: f64, h: f64) -> Vec<OuterRect> {
    // Reuse the existing strip algorithm. The total area
    // already equals w*h so the resulting rects naturally
    // fit; rows pack from top down. If the inputs slightly
    // under- or over-fill due to floating-point drift, the
    // last row absorbs the difference visually but it's
    // imperceptible at our scales.
    let _ = h;
    squarified_outer(areas, w)
}

/// Build just the tiles for one artifact's inner mosaic,
/// filling a `target_w × target_h` rect with the tile origin
/// at (0,0). Caller translates into the island's world
/// position.
fn build_artifact_tiles(
    bundle: &LoadedBundle,
    artifact: &glass_db::ArtifactId,
    target_w: f64,
    target_h: f64,
) -> Vec<WorldTile> {
    let Some(symbol_map) = bundle.symbol_maps.get(artifact) else {
        return Vec::new();
    };
    let mut inputs: Vec<LayoutInput> = symbol_map
        .iter()
        .filter(|s| matches!(s.kind, glass_arch_arm::SymbolKind::Function))
        .filter(|s| s.size > 0)
        .map(|s| {
            let section = find_containing_section(bundle, artifact, s.address)
                .unwrap_or_else(|| ".text".to_string());
            LayoutInput {
                artifact: artifact.clone(),
                section,
                addr: s.address,
                size: s.size,
                name: SharedString::from(s.display_name.clone()),
            }
        })
        .collect();
    if inputs.is_empty() || target_w <= 0. || target_h <= 0. {
        return Vec::new();
    }
    inputs.sort_by_key(|i| i.addr);

    let total_bytes: u64 = inputs.iter().map(|i| i.size).sum();
    if total_bytes == 0 {
        return Vec::new();
    }
    let target_area = target_w * target_h;
    let area_per_byte = target_area / (total_bytes as f64);
    let min_area = 4.0_f64;
    let areas: Vec<f64> = inputs
        .iter()
        .map(|i| ((i.size as f64) * area_per_byte).max(min_area))
        .collect();

    let (tiles, _h) = strip_layout(&inputs, &areas, target_w);
    tiles
}

/// One rectangle returned by the outer-island treemap.
#[derive(Clone, Debug)]
struct OuterRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Squarified treemap of the outer islands. Inputs are areas
/// (sorted desc by caller); output is a packed set of rects
/// fitting a width-`canvas_w` strip whose total height comes
/// out of the layout. Classic squarified algo: greedily build
/// row strips that minimise the worst aspect ratio.
fn squarified_outer(areas: Vec<f64>, canvas_w: f64) -> Vec<OuterRect> {
    let mut out: Vec<OuterRect> = Vec::with_capacity(areas.len());
    let mut cursor_y = 0.0_f64;
    let mut i = 0;
    while i < areas.len() {
        // Find the best strip: keep adding tiles while worst
        // aspect ratio improves.
        let mut best_k = 1;
        let mut best_score = f64::INFINITY;
        let mut running = 0.0;
        for k in 0..areas.len() - i {
            running += areas[i + k];
            let h = running / canvas_w;
            if h <= 0. {
                continue;
            }
            let smallest = areas[i..=i + k]
                .iter()
                .copied()
                .fold(f64::INFINITY, f64::min);
            let smallest_w = smallest / h;
            let aspect = (h / smallest_w).max(smallest_w / h);
            if aspect < best_score {
                best_score = aspect;
                best_k = k + 1;
            } else {
                // Aspect got worse — stop extending.
                break;
            }
        }
        let row_areas = &areas[i..i + best_k];
        let row_sum: f64 = row_areas.iter().sum();
        let h = (row_sum / canvas_w).max(8.0);
        let mut cursor_x = 0.0_f64;
        for a in row_areas {
            let w = a / h;
            out.push(OuterRect {
                x: cursor_x,
                y: cursor_y,
                w,
                h,
            });
            cursor_x += w;
        }
        cursor_y += h;
        i += best_k;
    }
    out
}

/// Input row for the layout algorithm.
struct LayoutInput {
    artifact: glass_db::ArtifactId,
    section: String,
    addr: u64,
    size: u64,
    name: SharedString,
}

/// Strip treemap. Lays out rectangles row-by-row, left-to-right
/// within each row, top-to-bottom across rows. Returns the
/// tiles + the final mosaic height.
fn strip_layout(
    inputs: &[LayoutInput],
    areas: &[f64],
    canvas_w: f64,
) -> (Vec<WorldTile>, f64) {
    let mut tiles = Vec::with_capacity(inputs.len());

    let mut cursor_y = 0.0_f64;
    let mut idx = 0;

    while idx < inputs.len() {
        let row_start = idx;
        let row_height = best_strip_height(&areas[idx..], canvas_w);
        let mut cursor_x = 0.0_f64;
        while idx < inputs.len() {
            let a = areas[idx];
            let tile_w = a / row_height;
            if cursor_x + tile_w > canvas_w + 0.5 && idx > row_start {
                break;
            }
            tiles.push(WorldTile {
                artifact: inputs[idx].artifact.clone(),
                section: inputs[idx].section.clone(),
                symbol_addr: inputs[idx].addr,
                symbol_size: inputs[idx].size,
                display_name: inputs[idx].name.clone(),
                wx: cursor_x as f32,
                wy: cursor_y as f32,
                ww: tile_w as f32,
                wh: row_height as f32,
            });
            cursor_x += tile_w;
            idx += 1;
            if cursor_x >= canvas_w {
                break;
            }
        }
        cursor_y += row_height;
    }

    (tiles, cursor_y)
}

/// Pick a strip height that gives the best worst-aspect-ratio
/// over the first few tiles in the strip. Bounded look-ahead
/// (16) keeps this O(n).
fn best_strip_height(areas_remaining: &[f64], canvas_w: f64) -> f64 {
    if areas_remaining.is_empty() {
        return 1.0;
    }
    let look = areas_remaining.len().min(16);
    let mut best_k = 1;
    let mut best_score = f64::INFINITY;
    let mut running_sum = 0.0;
    for k in 0..look {
        running_sum += areas_remaining[k];
        let h = running_sum / canvas_w;
        if h <= 0.0 {
            continue;
        }
        let smallest_area =
            areas_remaining[..=k].iter().copied().fold(f64::INFINITY, f64::min);
        let smallest_w = smallest_area / h;
        let aspect = (h / smallest_w).max(smallest_w / h);
        if aspect < best_score {
            best_score = aspect;
            best_k = k + 1;
        }
    }
    let chosen_sum: f64 = areas_remaining[..best_k].iter().sum();
    (chosen_sum / canvas_w).max(2.0)
}

fn find_containing_section(
    bundle: &LoadedBundle,
    artifact: &glass_db::ArtifactId,
    addr: u64,
) -> Option<String> {
    let sections = bundle.native_sections.get(artifact)?;
    for sec in sections {
        if addr >= sec.address && addr < sec.address + sec.size {
            return Some(sec.name.to_string());
        }
    }
    None
}

/// Map hits to a colour using a log-scaled cold→hot ramp.
/// `max_hits` is the recording's max, used as the ramp's top.
pub fn tile_colour(hits: Option<u32>, max_hits: u32) -> u32 {
    const NEUTRAL: u32 = 0x2a2e35;
    const COLD: u32 = 0x1a2436;
    const BLUE: u32 = 0x1f3b8a;
    const YELLOW: u32 = 0xe1c542;
    const RED: u32 = 0xd13b3b;

    let Some(h) = hits else {
        return NEUTRAL;
    };
    if h == 0 {
        return COLD;
    }
    let t = if max_hits <= 1 {
        1.0
    } else {
        let num = (h as f32).max(1.0).log2();
        let den = (max_hits as f32).max(2.0).log2();
        (num / den).clamp(0.0, 1.0)
    };
    if t < 0.5 {
        lerp_rgb(BLUE, YELLOW, t * 2.0)
    } else {
        lerp_rgb(YELLOW, RED, (t - 0.5) * 2.0)
    }
}

fn lerp_rgb(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let chan = |sh: u32| {
        let ac = ((a >> sh) & 0xffu32) as f32;
        let bc = ((b >> sh) & 0xffu32) as f32;
        ((ac + (bc - ac) * t) as u32) & 0xffu32
    };
    (chan(16) << 16) | (chan(8) << 8) | chan(0)
}

/// Project a world rectangle to screen-pixel coords. Returns
/// `None` if the rect is fully outside the viewport. Used for
/// islands; tiles project through `project_tile` which carries
/// extra per-tile fields.
fn project_rect(
    wx: f32,
    wy: f32,
    ww: f32,
    wh: f32,
    camera: &CoverageCamera,
) -> Option<ScreenRect> {
    let bounds = camera.viewport_bounds;
    let vw = bounds.size.width.as_f32();
    let vh = bounds.size.height.as_f32();
    if vw <= 0. || vh <= 0. {
        return None;
    }
    let centre_x = vw / 2.;
    let centre_y = vh / 2.;
    let unit = camera.unit();
    let sx = centre_x + (wx - camera.pan_x) * unit;
    let sy = centre_y + (wy - camera.pan_y) * unit;
    let sw = ww * unit;
    let sh = wh * unit;
    if sx + sw < 0. || sx > vw || sy + sh < 0. || sy > vh {
        return None;
    }
    Some(ScreenRect { x: sx, y: sy, w: sw, h: sh })
}

#[derive(Clone, Copy, Debug)]
struct ScreenRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Transform a world tile to screen coords using the camera.
/// Returns `None` if the tile is fully outside the viewport.
fn project_tile(world: &WorldTile, camera: &CoverageCamera) -> Option<ScreenTile> {
    let bounds = camera.viewport_bounds;
    let vw = bounds.size.width.as_f32();
    let vh = bounds.size.height.as_f32();
    if vw <= 0. || vh <= 0. {
        return None;
    }
    let centre_x = vw / 2.;
    let centre_y = vh / 2.;
    let unit = camera.unit();
    let sx = centre_x + (world.wx - camera.pan_x) * unit;
    let sy = centre_y + (world.wy - camera.pan_y) * unit;
    let sw = world.ww * unit;
    let sh = world.wh * unit;
    // Cull tiles that don't intersect [0,vw]×[0,vh].
    if sx + sw < 0. || sx > vw || sy + sh < 0. || sy > vh {
        return None;
    }
    Some(ScreenTile {
        artifact: world.artifact.clone(),
        section: world.section.clone(),
        symbol_addr: world.symbol_addr,
        display_name: world.display_name.clone(),
        x: sx,
        y: sy,
        w: sw,
        h: sh,
    })
}

/// Render the CoverageMap tab body — global view with one
/// island per native artifact.
pub fn render_coverage_tab(
    shell: &mut Shell,
    bundle: LoadedBundle,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    let any_native = !bundle.native_artifact_labels.is_empty();
    let any_dex = !bundle.smali_classes.is_empty();
    let any_code = any_native || any_dex;

    // Peek the cached layout (if any) for the chosen-ABI
    // header text. First paint on a fresh bundle has no
    // cached layout yet → use a generic header.
    let header_text = if !any_code {
        "Coverage Map — bundle has no native or DEX code".to_string()
    } else {
        let abi_summary = shell.coverage_layout.as_ref().and_then(|(_, l)| {
            l.chosen_abi.map(|k| {
                if l.hidden_artifact_count > 0 {
                    format!(
                        "ABI: {} (hiding {} other-ABI artifact{})",
                        k.short_label(),
                        l.hidden_artifact_count,
                        if l.hidden_artifact_count == 1 { "" } else { "s" },
                    )
                } else {
                    format!("ABI: {}", k.short_label())
                }
            })
        });
        match abi_summary {
            Some(s) => format!(
                "Coverage Map — {s} — drag to pan, ⌘/Ctrl-scroll to zoom"
            ),
            None => "Coverage Map — drag to pan, ⌘/Ctrl-scroll to zoom".to_string(),
        }
    };

    let header = div()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(border)
        .text_color(fg)
        .child(SharedString::from(header_text));

    let body: gpui::AnyElement = if any_code {
        render_canvas(shell, bundle, panel, border, dim, fg, cx)
    } else {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(dim)
            .child("This bundle has no code to map.")
            .into_any_element()
    };

    div()
        .flex_1()
        .flex()
        .flex_col()
        .min_h_0()
        .child(header)
        .child(body)
        .into_any_element()
}

/// Build (or fetch the cached) global layout, project each
/// tile + island header through the camera, and render the
/// resulting screen elements plus the pan/zoom event handlers.
fn render_canvas(
    shell: &mut Shell,
    bundle: LoadedBundle,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    dim: gpui::Rgba,
    fg: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    // Layout once per bundle, stash on Shell. Key the cache
    // by `bundle_id` (or `display_label` as a fallback) so a
    // reopen invalidates it.
    let cache_key = bundle
        .bundle_id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_else(|| bundle.display_label.clone());
    let needs_relayout = shell
        .coverage_layout
        .as_ref()
        .map(|(k, _)| k != &cache_key)
        .unwrap_or(true);
    if needs_relayout {
        let layout = build_mosaic_global(&bundle);
        shell.coverage_layout = Some((cache_key, Arc::new(layout)));
    }
    let layout = shell
        .coverage_layout
        .as_ref()
        .map(|(_, l)| l.clone())
        .unwrap();

    // First fit-to-view: defer until the canvas hook has
    // written real bounds. Once we've seen bounds and layout
    // has dimensions, snap zoom + pan so the whole mosaic is
    // visible.
    if !shell.coverage_camera.initialised
        && shell.coverage_camera.viewport_bounds.size.width.as_f32() > 0.
        && layout.world_w > 0.
        && layout.world_h > 0.
    {
        shell
            .coverage_camera
            .fit_to(layout.world_w, layout.world_h);
    }

    let camera = shell.coverage_camera.clone();

    // Project tiles. Empty layout → placeholder text.
    if layout.tiles.is_empty() {
        return div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(dim)
            .child("No function symbols in this artifact.")
            .into_any_element();
    }

    let weak = cx.entity().downgrade();

    // Measure: capture bounds → push to camera.
    let measure_weak = weak.clone();
    let measure = gpui::canvas(
        move |bounds, _window, cx| {
            if let Some(entity) = measure_weak.upgrade() {
                cx.update_entity(&entity, |shell, cx| {
                    let cur = shell.coverage_camera.viewport_bounds;
                    let changed = (cur.size.width.as_f32()
                        - bounds.size.width.as_f32())
                    .abs()
                        + (cur.size.height.as_f32()
                            - bounds.size.height.as_f32())
                        .abs();
                    shell.coverage_camera.viewport_bounds = bounds;
                    if changed > 0.5 {
                        cx.notify();
                    }
                });
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    // Island layer: bounding rect + header strip per island.
    // Drawn under the tile layer so tiles sit on top of the
    // header tint.
    let mut island_layer = div().absolute().top_0().left_0().size_full();
    for isl in &layout.islands {
        let Some(rect) = project_rect(
            isl.wx,
            isl.wy,
            isl.ww,
            isl.wh,
            &camera,
        ) else {
            continue;
        };
        if rect.w < 4. || rect.h < 4. {
            continue;
        }
        // Outer border for the island. Background = panel
        // colour so the gaps between tiles show through as
        // water; the tiles themselves carry the kind tint.
        let outer = div()
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.w))
            .h(px(rect.h))
            .border_1()
            .border_color(border)
            .bg(panel);
        island_layer = island_layer.child(outer);

        // Header strip. Skip when zoomed out so far that
        // the header would dominate the island.
        let unit = camera.unit();
        let header_screen_h = isl.header_h * unit;
        if header_screen_h >= 10. && rect.w >= 40. {
            let hdr_h = header_screen_h.min(rect.h * 0.4);
            let label_visible = rect.w >= 80. && hdr_h >= 12.;
            let label_text = SharedString::from(format!(
                "{}  ·  {}",
                isl.kind.short_label(),
                isl.label,
            ));
            let mut hdr = div()
                .absolute()
                .left(px(rect.x))
                .top(px(rect.y))
                .w(px(rect.w))
                .h(px(hdr_h))
                .bg(rgb(island_bg(isl.kind)))
                .border_b_1()
                .border_color(border);
            if label_visible {
                hdr = hdr.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(rgb(0xeeeeee))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(label_text),
                );
            }
            island_layer = island_layer.child(hdr);
        }
    }

    // Tile layer.
    let mut tile_layer = div().absolute().top_0().left_0().size_full();
    let mut visible = 0usize;
    for world in &layout.tiles {
        let Some(s) = project_tile(world, &camera) else {
            continue;
        };
        visible += 1;
        // Skip sub-pixel tiles entirely — they'd render as
        // invisible smudges and burn the layout cost.
        if s.w < 0.5 || s.h < 0.5 {
            continue;
        }
        // Tiles carry the island's kind tint so each island
        // reads as a region of its colour. Native tiles =
        // warm brown; DEX class tiles = forest green
        // (handled in the DEX branch below).
        let bg = ISLAND_BG_NATIVE;
        let label = if s.w >= 60. && s.h >= 14. {
            Some(s.display_name.clone())
        } else {
            None
        };
        let artifact_click = s.artifact.clone();
        let section_click = s.section.clone();
        let addr_click = s.symbol_addr;
        let mut t = div()
            .absolute()
            .left(px(s.x))
            .top(px(s.y))
            .w(px(s.w.max(1.0)))
            .h(px(s.h.max(1.0)))
            .bg(rgb(bg))
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                    let new_tab = ev.modifiers.shift;
                    if new_tab {
                        this.open_listing_force_new_tab(
                            artifact_click.clone(),
                            section_click.clone(),
                            addr_click,
                            cx,
                        );
                    } else {
                        this.open_listing_at(
                            artifact_click.clone(),
                            section_click.clone(),
                            addr_click,
                            cx,
                        );
                    }
                    // Don't bubble to the background drag handler.
                    cx.stop_propagation();
                }),
            );
        if let Some(text) = label {
            t = t.child(
                div()
                    .px_1()
                    .text_xs()
                    .text_color(rgb(0xffffff))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(text),
            );
        }
        tile_layer = tile_layer.child(t);
    }
    let _ = visible;

    // DEX content layer. For each DEX island, render its
    // packages — and within each package, either the
    // individual class tiles (when zoomed in enough that
    // classes are visible) or a single package summary tile
    // (when zoomed out). The threshold: a package's smallest
    // dimension on screen has to exceed `CLASS_LOD_PX` for us
    // to drop into classes; otherwise the whole package is
    // one tile.
    const CLASS_LOD_PX: f32 = 60.0;
    for (idx, content_opt) in layout.dex_content.iter().enumerate() {
        let Some(content) = content_opt.as_ref() else { continue };
        let Some(isl) = layout.islands.get(idx) else { continue };
        // Per-island culling — if the island isn't on screen
        // at all, skip its content.
        if project_rect(isl.wx, isl.wy, isl.ww, isl.wh, &camera).is_none() {
            continue;
        }
        for pkg in &content.packages {
            let Some(pkg_screen) =
                project_rect(pkg.wx, pkg.wy, pkg.ww, pkg.wh, &camera)
            else {
                continue;
            };
            if pkg_screen.w < 2.0 || pkg_screen.h < 2.0 {
                continue;
            }
            let render_classes = pkg_screen.w >= CLASS_LOD_PX
                && pkg_screen.h >= CLASS_LOD_PX;
            if render_classes {
                // Zoomed-in branch: render every class.
                for class in &pkg.classes {
                    let Some(cs) = project_rect(
                        class.wx, class.wy, class.ww, class.wh, &camera,
                    ) else {
                        continue;
                    };
                    if cs.w < 0.5 || cs.h < 0.5 {
                        continue;
                    }
                    let bg = ISLAND_BG_DEX;
                    let label = if cs.w >= 50. && cs.h >= 14. {
                        Some(class.display_name.clone())
                    } else {
                        None
                    };
                    let jni_for_click = class.class_jni.clone();
                    let mut t = div()
                        .absolute()
                        .left(px(cs.x))
                        .top(px(cs.y))
                        .w(px(cs.w.max(1.0)))
                        .h(px(cs.h.max(1.0)))
                        .bg(rgb(bg))
                        .border_1()
                        .border_color(border)
                        .cursor_pointer()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(
                                move |this,
                                      _ev: &gpui::MouseDownEvent,
                                      _w,
                                      cx| {
                                    this.open_smali_editor_for_class(
                                        &jni_for_click,
                                        cx,
                                    );
                                    cx.stop_propagation();
                                },
                            ),
                        );
                    if let Some(text) = label {
                        t = t.child(
                            div()
                                .px_1()
                                .text_xs()
                                .text_color(rgb(0xffffff))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(text),
                        );
                    }
                    tile_layer = tile_layer.child(t);
                }
                // Faint package outline so the user can see
                // where packages start/end when classes fill
                // them. No bg — just a border.
                let outline = div()
                    .absolute()
                    .left(px(pkg_screen.x))
                    .top(px(pkg_screen.y))
                    .w(px(pkg_screen.w))
                    .h(px(pkg_screen.h))
                    .border_1()
                    .border_color(rgba_dim_border());
                tile_layer = tile_layer.child(outline);
                // Package label badge if the package rect is
                // big enough — drawn at the top-left as a
                // small chip so it doesn't fight the class
                // labels.
                if pkg_screen.w >= 100. && pkg_screen.h >= 24. {
                    let pkg_label = if pkg.name.is_empty() {
                        SharedString::from("(default)")
                    } else {
                        pkg.name.clone()
                    };
                    // Chip = panel-colour pill so it reads as
                    // a label badge over the green tiles
                    // underneath.
                    let chip = div()
                        .absolute()
                        .left(px(pkg_screen.x + 2.))
                        .top(px(pkg_screen.y + 2.))
                        .px_1()
                        .text_xs()
                        .text_color(rgb(0xcccccc))
                        .bg(panel)
                        .child(pkg_label);
                    tile_layer = tile_layer.child(chip);
                }
            } else {
                // Zoomed-out branch: one tile per package.
                let bg = ISLAND_BG_DEX;
                let label = if pkg_screen.w >= 60. && pkg_screen.h >= 14. {
                    let pkg_label = if pkg.name.is_empty() {
                        SharedString::from("(default)")
                    } else {
                        pkg.name.clone()
                    };
                    Some(pkg_label)
                } else {
                    None
                };
                let mut t = div()
                    .absolute()
                    .left(px(pkg_screen.x))
                    .top(px(pkg_screen.y))
                    .w(px(pkg_screen.w.max(1.0)))
                    .h(px(pkg_screen.h.max(1.0)))
                    .bg(rgb(bg))
                    .border_1()
                    .border_color(border);
                if let Some(text) = label {
                    t = t.child(
                        div()
                            .px_1()
                            .text_xs()
                            .text_color(rgb(0xffffff))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(text),
                    );
                }
                tile_layer = tile_layer.child(t);
            }
        }
    }

    // Event-handler wrapper: wheel pan / mod-wheel zoom /
    // middle-drag pan / left-drag-on-background pan.
    let wheel_weak = weak.clone();
    let down_weak = weak.clone();
    let mid_down_weak = weak.clone();
    let move_weak = weak.clone();
    let up_weak = weak.clone();

    div()
        .flex_1()
        .relative()
        .overflow_hidden()
        .child(measure)
        .child(island_layer)
        .child(tile_layer)
        .on_scroll_wheel(move |ev: &gpui::ScrollWheelEvent, _w, cx: &mut App| {
            let Some(entity) = wheel_weak.upgrade() else { return };
            let pos = ev.position;
            let delta = ev.delta.pixel_delta(px(20.));
            let zoom = ev.modifiers.shift
                || ev.modifiers.platform
                || ev.modifiers.control;
            cx.update_entity(&entity, |shell, cx| {
                if zoom {
                    let raw = if delta.y.as_f32().abs() > 0. {
                        delta.y.as_f32()
                    } else {
                        delta.x.as_f32()
                    };
                    shell.coverage_camera.zoom_by(pos, raw);
                } else {
                    shell
                        .coverage_camera
                        .pan_by(delta.x.as_f32(), delta.y.as_f32());
                }
                cx.notify();
            });
        })
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                // Background drag: only fires when the tile's
                // own handler did NOT stop_propagation, so a
                // click on a tile won't start a drag here.
                let Some(entity) = down_weak.upgrade() else { return };
                let pos = ev.position;
                cx.update_entity(&entity, |shell, _cx| {
                    shell.coverage_camera.drag_start(pos);
                });
            },
        )
        .on_mouse_down(
            gpui::MouseButton::Middle,
            move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                let Some(entity) = mid_down_weak.upgrade() else { return };
                let pos = ev.position;
                cx.update_entity(&entity, |shell, _cx| {
                    shell.coverage_camera.drag_start(pos);
                });
            },
        )
        .on_mouse_move(move |ev: &gpui::MouseMoveEvent, _w, cx: &mut App| {
            let Some(entity) = move_weak.upgrade() else { return };
            let pos = ev.position;
            cx.update_entity(&entity, |shell, cx| {
                if shell.coverage_camera.drag_start.is_some() {
                    shell.coverage_camera.drag_move(pos);
                    cx.notify();
                }
            });
        })
        .on_mouse_up(
            gpui::MouseButton::Left,
            {
                let up_weak = up_weak.clone();
                move |_ev: &gpui::MouseUpEvent, _w, cx: &mut App| {
                    let Some(entity) = up_weak.upgrade() else { return };
                    cx.update_entity(&entity, |shell, _cx| {
                        shell.coverage_camera.drag_end();
                    });
                }
            },
        )
        .on_mouse_up(
            gpui::MouseButton::Middle,
            move |_ev: &gpui::MouseUpEvent, _w, cx: &mut App| {
                let Some(entity) = up_weak.upgrade() else { return };
                cx.update_entity(&entity, |shell, _cx| {
                    shell.coverage_camera.drag_end();
                });
            },
        )
        .into_any_element()
}
