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
const ZOOM_STEP: f32 = 1.1;

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
    pub tiles: Vec<WorldTile>,
    pub islands: Vec<Island>,
    pub world_w: f32,
    pub world_h: f32,
}

/// One island in the global view — one per native artifact.
/// Contains the artifact's label and its bounding rect in
/// world space (used to draw the header strip and the
/// outer border).
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
    /// kind-tinted background so similar ABIs read as one
    /// neighbourhood at a glance.
    pub header_h: f32,
}

/// What kind of code an island contains. Drives the header
/// tint so the user can tell ABI groups apart visually.
/// DEX is reserved for when we add Java-side coverage.
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

    /// Header background tint. Picked to be muted enough that
    /// the hot/cold tile colours still read on top.
    pub fn header_colour(self) -> u32 {
        match self {
            Self::NativeArm64 => 0x2a3a4f, // muted teal-blue
            Self::NativeArm => 0x2f3a47,  // slightly cooler
            Self::NativeX86_64 => 0x3f3a2a, // muted amber
            Self::NativeX86 => 0x47402f,  // amber, dimmer
            Self::NativeOther => 0x383838, // neutral grey
            Self::Dex => 0x3a2f47,         // muted purple (reserved)
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

impl MosaicLayout {
    pub fn empty() -> Self {
        Self {
            tiles: Vec::new(),
            islands: Vec::new(),
            world_w: 0.,
            world_h: 0.,
        }
    }
}

/// Build the global mosaic — one island per native artifact,
/// laid out by a squarified outer treemap (sized by total
/// `.text` bytes). Inside each island, an address-ordered
/// strip-treemap of the artifact's functions.
///
/// Empty bundle (no native code) → empty layout.
pub fn build_mosaic_global(bundle: &LoadedBundle) -> MosaicLayout {
    // Gather candidate artifacts: anything with native
    // function symbols. The label is the artifact's APK path
    // (`lib/arm64-v8a/libfoo.so`) so the user can tell ABIs
    // apart.
    let mut candidates: Vec<(glass_db::ArtifactId, u64, SharedString)> =
        Vec::new();
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
        candidates.push((aid.clone(), total_bytes, label));
    }
    if candidates.is_empty() {
        return MosaicLayout::empty();
    }

    // Outer treemap. World rect target: fixed 1200-wide
    // landscape, height comes out of the layout. Per-island
    // area is proportional to that island's bytes.
    let outer_world_w = 1200.0_f64;
    let outer_aspect = 4.0_f64 / 3.0;
    let outer_target_h = outer_world_w / outer_aspect;
    let outer_area = outer_world_w * outer_target_h;
    let total_bytes: u64 = candidates.iter().map(|(_, b, _)| *b).sum();
    if total_bytes == 0 {
        return MosaicLayout::empty();
    }

    // For the outer level we *do* size-sort: islands have no
    // intrinsic ordering, so picking the best aspect ratios
    // wins. Sort desc by bytes.
    let mut outer_inputs: Vec<(usize, f64)> = candidates
        .iter()
        .enumerate()
        .map(|(i, (_, b, _))| {
            let area = (*b as f64) / (total_bytes as f64) * outer_area;
            (i, area)
        })
        .collect();
    outer_inputs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let outer_rects = squarified_outer(
        outer_inputs.iter().map(|(_, a)| *a).collect(),
        outer_world_w,
    );
    // Outer height = max y+h across rects.
    let mut max_h = 0.0_f64;
    for r in &outer_rects {
        let bottom = r.y + r.h;
        if bottom > max_h {
            max_h = bottom;
        }
    }
    let outer_world_h = max_h;

    // Now build each island's inner mosaic. The island's
    // inner rect is the outer rect minus a header strip and
    // a small margin.
    let header_h_world = 24.0_f64; // ~24 world units = 24 px @ zoom 1
    let margin_world = 4.0_f64;

    let mut all_tiles: Vec<WorldTile> = Vec::new();
    let mut islands: Vec<Island> = Vec::new();
    for (i, (orig_idx, _area)) in outer_inputs.iter().enumerate() {
        let r = &outer_rects[i];
        let (aid, _bytes, label) = &candidates[*orig_idx];
        let inner_x = r.x + margin_world;
        let inner_y = r.y + header_h_world + margin_world;
        let inner_w = (r.w - 2.0 * margin_world).max(8.0);
        let inner_h = (r.h - header_h_world - 2.0 * margin_world).max(8.0);

        let inner_tiles = build_artifact_tiles(bundle, aid, inner_w, inner_h);
        // Translate each tile from inner-local coords to
        // world coords.
        for mut t in inner_tiles {
            t.wx += inner_x as f32;
            t.wy += inner_y as f32;
            all_tiles.push(t);
        }
        let kind = IslandKind::from_label(label.as_ref());
        islands.push(Island {
            artifact: aid.clone(),
            label: label.clone(),
            kind,
            wx: r.x as f32,
            wy: r.y as f32,
            ww: r.w as f32,
            wh: r.h as f32,
            header_h: header_h_world as f32,
        });
    }

    MosaicLayout {
        tiles: all_tiles,
        islands,
        world_w: outer_world_w as f32,
        world_h: outer_world_h as f32,
    }
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
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    let any_native = !bundle.native_artifact_labels.is_empty();

    let header_text = if any_native {
        "Coverage Map — drag to pan, ⌘/Ctrl-scroll to zoom"
    } else {
        "Coverage Map — no native code in this bundle"
    };

    let header = div()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(border)
        .text_color(fg)
        .child(SharedString::from(header_text.to_string()));

    let body: gpui::AnyElement = if any_native {
        render_canvas(shell, bundle, border, dim, fg, cx)
    } else {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(dim)
            .child(
                "No native code. Coverage requires native instrumentation; \
                 DEX/Java-side coverage is on the roadmap.",
            )
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
        // Outer border for the island.
        let outer = div()
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.w))
            .h(px(rect.h))
            .border_1()
            .border_color(border)
            .bg(rgb(isl.kind.header_colour()));
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
                .bg(rgb(isl.kind.header_colour()))
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
        let bg = tile_colour(None, 1);
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
