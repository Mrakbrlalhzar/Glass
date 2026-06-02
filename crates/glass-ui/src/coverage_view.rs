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
    pub world_w: f32,
    pub world_h: f32,
}

impl MosaicLayout {
    pub fn empty() -> Self {
        Self {
            tiles: Vec::new(),
            world_w: 0.,
            world_h: 0.,
        }
    }
}

/// Build the world-space mosaic for one native artifact. The
/// target world width is fixed at `world_w`; the height comes
/// out of the strip layout so the area encodes total bytes.
pub fn build_mosaic(
    bundle: &LoadedBundle,
    artifact: &glass_db::ArtifactId,
    world_w: f32,
) -> MosaicLayout {
    let Some(symbol_map) = bundle.symbol_maps.get(artifact) else {
        return MosaicLayout::empty();
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
    if inputs.is_empty() || world_w <= 0. {
        return MosaicLayout::empty();
    }
    inputs.sort_by_key(|i| i.addr);

    // Pick a target world aspect ratio so the mosaic doesn't
    // look like a long thin strip. We bias slightly wider than
    // tall (most viewports are landscape). The actual height
    // falls out from the strip layout once we set the per-byte
    // area scale to satisfy world_w × world_h ≈ total_bytes.
    let total_bytes: u64 = inputs.iter().map(|i| i.size).sum();
    if total_bytes == 0 {
        return MosaicLayout::empty();
    }
    let target_aspect = 4.0_f64 / 3.0;
    let target_h = (world_w as f64) / target_aspect;
    let target_area = (world_w as f64) * target_h;
    let area_per_byte = target_area / (total_bytes as f64);
    // Minimum tile area in world units: 4 (=2×2 world units).
    // At default zoom that's 2×2 px — clickable enough once
    // the user zooms in, invisible-ish when zoomed out.
    let min_area = 4.0_f64;

    let areas: Vec<f64> = inputs
        .iter()
        .map(|i| ((i.size as f64) * area_per_byte).max(min_area))
        .collect();

    let (tiles, world_h) = strip_layout(&inputs, &areas, world_w as f64);
    MosaicLayout {
        tiles,
        world_w,
        world_h: world_h as f32,
    }
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

/// Render the CoverageMap tab body.
pub fn render_coverage_tab(
    shell: &mut Shell,
    bundle: LoadedBundle,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    let artifact = pick_default_artifact(&bundle);
    let artifact_label = artifact
        .as_ref()
        .and_then(|a| bundle.native_artifact_labels.get(a))
        .cloned()
        .unwrap_or_else(|| "(no native artifact)".to_string());

    let header_text = match &artifact {
        Some(_) => format!(
            "Coverage Map — {artifact_label}  (drag to pan, ⌘/Ctrl-scroll to zoom)"
        ),
        None => "Coverage Map — no native artifacts in this bundle".to_string(),
    };

    let header = div()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(border)
        .text_color(fg)
        .child(SharedString::from(header_text));

    let body: gpui::AnyElement = match artifact {
        Some(aid) => render_canvas(shell, bundle, aid, border, dim, cx),
        None => div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(dim)
            .child(
                "This bundle has no native code. Coverage requires native instrumentation.",
            )
            .into_any_element(),
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

fn pick_default_artifact(bundle: &LoadedBundle) -> Option<glass_db::ArtifactId> {
    let mut best: Option<(glass_db::ArtifactId, u64)> = None;
    for (aid, sections) in bundle.native_sections.iter() {
        let text_bytes: u64 = sections
            .iter()
            .filter(|s| matches!(s.kind, crate::NativeSectionKind::Text))
            .map(|s| s.size)
            .sum();
        if text_bytes == 0 {
            continue;
        }
        match &best {
            None => best = Some((aid.clone(), text_bytes)),
            Some((_, prev)) if text_bytes > *prev => {
                best = Some((aid.clone(), text_bytes));
            }
            _ => {}
        }
    }
    best.map(|(a, _)| a)
}

/// Build (or fetch the cached) world layout for the artifact,
/// project it through the camera, and render the resulting
/// screen tiles plus the pan/zoom event handlers.
fn render_canvas(
    shell: &mut Shell,
    bundle: LoadedBundle,
    artifact: glass_db::ArtifactId,
    border: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    // Layout once and stash on Shell. Re-layout if the cached
    // artifact differs (user might switch artifacts later via
    // a dropdown).
    let needs_relayout = shell
        .coverage_layout
        .as_ref()
        .map(|(aid, _)| aid != &artifact)
        .unwrap_or(true);
    if needs_relayout {
        let layout = build_mosaic(&bundle, &artifact, 1200.0);
        shell.coverage_layout = Some((artifact.clone(), Arc::new(layout)));
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
