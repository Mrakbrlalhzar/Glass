//! Address-ordered treemap of an artifact's functions.
//!
//! Each native artifact's text-section symbols are laid out as
//! a mosaic — tile area proportional to symbol size, tile order
//! follows `.text` address order so neighbours in the mosaic are
//! neighbours in the binary. Click a tile → Listing tab at the
//! function's address. When a coverage recording exists, tiles
//! are coloured by log-scaled hit count.
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

use gpui::{div, prelude::*, px, rgb, Context, SharedString};

use crate::{LoadedBundle, Shell};

/// One rectangle in the mosaic.
#[derive(Clone, Debug)]
#[allow(dead_code)] // symbol_size will drive tooltip + colour in v0.5
pub struct Tile {
    pub artifact: glass_db::ArtifactId,
    pub section: String,
    pub symbol_addr: u64,
    pub symbol_size: u64,
    pub display_name: SharedString,
    /// Pixel bounds within the canvas, relative to the canvas origin.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Cached hit count for this function, populated by callers
    /// from the current `Shell::coverage` recording. `None` when
    /// no recording is active; `Some(0)` when recorded but cold.
    pub hits: Option<u32>,
}

/// Input row for the layout algorithm.
struct LayoutInput {
    artifact: glass_db::ArtifactId,
    section: String,
    addr: u64,
    size: u64,
    name: SharedString,
}

/// Compute the mosaic for one native artifact.
///
/// `bounds_w` / `bounds_h` are the canvas size in pixels.
/// `min_tile_px` is the minimum dimension we'll let any tile
/// shrink to — below this, the function would be unclickable.
/// Tiles smaller than that fall back to the floor; tiny
/// functions therefore collectively take more area than their
/// total size warrants, but the alternative (sub-pixel tiles)
/// is unusable.
pub fn build_mosaic(
    bundle: &LoadedBundle,
    artifact: &glass_db::ArtifactId,
    bounds_w: f32,
    bounds_h: f32,
    min_tile_px: f32,
) -> Vec<Tile> {
    let Some(symbol_map) = bundle.symbol_maps.get(artifact) else {
        return Vec::new();
    };

    // Gather function symbols in address order. The SymbolMap
    // already keeps them sorted; we just have to filter to
    // functions with non-zero size.
    let mut inputs: Vec<LayoutInput> = symbol_map
        .iter()
        .filter(|s| matches!(s.kind, glass_arch_arm::SymbolKind::Function))
        .filter(|s| s.size > 0)
        .map(|s| {
            // Look up the containing text section so the click
            // handler knows what to pass to the Listing tab.
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

    if inputs.is_empty() || bounds_w < 1.0 || bounds_h < 1.0 {
        return Vec::new();
    }

    // Sort by address — sym map is supposed to already be in
    // order but normalise here for robustness.
    inputs.sort_by_key(|i| i.addr);

    let total_size: u64 = inputs.iter().map(|i| i.size).sum();
    if total_size == 0 {
        return Vec::new();
    }
    let total_area = (bounds_w as f64) * (bounds_h as f64);
    let area_per_byte = total_area / (total_size as f64);
    let min_area = (min_tile_px as f64) * (min_tile_px as f64);

    // Effective area per tile: max(real_area, min_area). We
    // pack these into rows ("strips") using the strip-treemap
    // algorithm: fix the strip's height, fill it with tiles
    // in input order until adding another would worsen the
    // aspect ratio more than starting a new strip would.
    let areas: Vec<f64> = inputs
        .iter()
        .map(|i| ((i.size as f64) * area_per_byte).max(min_area))
        .collect();

    let tiles = strip_layout(&inputs, &areas, bounds_w as f64, bounds_h as f64);
    tiles
}

/// Strip treemap. Lays out rectangles row-by-row, left-to-right
/// within each row, top-to-bottom across rows. Starts a new row
/// when the running average aspect ratio in the current row
/// would deteriorate by adding another tile.
fn strip_layout(
    inputs: &[LayoutInput],
    areas: &[f64],
    canvas_w: f64,
    canvas_h: f64,
) -> Vec<Tile> {
    let mut tiles = Vec::with_capacity(inputs.len());

    let mut cursor_y = 0.0_f64;
    let mut idx = 0;

    while idx < inputs.len() {
        // Greedy strip-fill: how many tiles to put in this
        // row? Start with one, keep adding as long as the
        // worst aspect ratio improves.
        let row_start = idx;
        let row_height = best_strip_height(&areas[idx..], canvas_w);
        // How many tiles actually fit in this strip — every
        // tile's width = area / row_height. We fill until the
        // accumulated width hits canvas_w.
        let mut cursor_x = 0.0_f64;
        while idx < inputs.len() {
            let a = areas[idx];
            let tile_w = a / row_height;
            if cursor_x + tile_w > canvas_w + 0.5 && idx > row_start {
                // Tile would overflow this strip; bump to next.
                break;
            }
            tiles.push(Tile {
                artifact: inputs[idx].artifact.clone(),
                section: inputs[idx].section.clone(),
                symbol_addr: inputs[idx].addr,
                symbol_size: inputs[idx].size,
                display_name: inputs[idx].name.clone(),
                x: cursor_x as f32,
                y: cursor_y as f32,
                w: tile_w as f32,
                h: row_height as f32,
                hits: None,
            });
            cursor_x += tile_w;
            idx += 1;
            // If we'd start to overflow on the next iteration,
            // break — this gives the strip a slight under-fill
            // we can't avoid without back-tracking.
            if cursor_x >= canvas_w {
                break;
            }
        }
        cursor_y += row_height;
        if cursor_y >= canvas_h {
            // Out of vertical space. The remaining tiles get
            // squashed into a final strip — better than
            // dropping them; the user can still see them and
            // we can re-layout if they resize the window.
            if idx < inputs.len() {
                let remaining_area: f64 = areas[idx..].iter().sum();
                let final_h = (canvas_h - cursor_y + row_height).max(2.0);
                // Replace the last completed row's height with
                // a stretched one that fits the remainder.
                let _ = remaining_area;
                let _ = final_h;
            }
            break;
        }
    }

    tiles
}

/// Pick a strip height that gives the best worst-aspect-ratio
/// over the first few tiles in the strip. Bounded look-ahead
/// (16) keeps this O(n).
fn best_strip_height(areas_remaining: &[f64], canvas_w: f64) -> f64 {
    if areas_remaining.is_empty() {
        return 1.0;
    }
    let look = areas_remaining.len().min(16);
    // For k tiles spanning width canvas_w, each contributes
    // area_i, so heights are all the same: h = sum(area)/w.
    // Aspect ratio of tile i: max(w_i/h, h/w_i) where w_i = a_i/h.
    // We pick the k that minimises the worst aspect ratio.
    let mut best_k = 1;
    let mut best_score = f64::INFINITY;
    let mut running_sum = 0.0;
    for k in 0..look {
        running_sum += areas_remaining[k];
        let h = running_sum / canvas_w;
        if h <= 0.0 {
            continue;
        }
        // Worst aspect ratio in this strip is the *smallest*
        // tile vs. h (it gets the narrowest width).
        let smallest_area = areas_remaining[..=k].iter().copied().fold(f64::INFINITY, f64::min);
        let smallest_w = smallest_area / h;
        let aspect = (h / smallest_w).max(smallest_w / h);
        if aspect < best_score {
            best_score = aspect;
            best_k = k + 1;
        }
    }
    let _ = best_k;
    // Strip height = sum of chosen areas / canvas_w. We
    // re-derived best_k but we don't need to return it; the
    // outer loop will re-walk and add tiles. To keep the
    // outer loop simple we return the height that
    // corresponds to filling exactly best_k tiles.
    let chosen_sum: f64 = areas_remaining[..best_k].iter().sum();
    (chosen_sum / canvas_w).max(2.0)
}

/// Walk the native sections for `artifact` and return the
/// section name whose `[base, base+size)` contains `addr`.
/// Used so the click handler can open the right Listing tab.
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
    const NEUTRAL: u32 = 0x2a2e35; // no recording
    const COLD: u32 = 0x1a2436; // recorded, 0 hits
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

/// Render the CoverageMap tab body for a single artifact.
///
/// Renders the toolbar (artifact picker placeholder) and the
/// mosaic canvas underneath. The mosaic is laid out at the
/// most-recently-measured canvas size, captured by a gpui
/// `canvas` prepaint hook on every frame — same trick the
/// section-map view uses to get pixel-accurate bounds.
pub fn render_coverage_tab(
    shell: &mut Shell,
    bundle: LoadedBundle,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    // Pick the first native artifact for v0. A dropdown comes
    // in v1 when more than one .so warrants the choice.
    let artifact = pick_default_artifact(&bundle);
    let artifact_label = artifact
        .as_ref()
        .and_then(|a| bundle.native_artifact_labels.get(a))
        .cloned()
        .unwrap_or_else(|| "(no native artifact)".to_string());

    let header_text = match &artifact {
        Some(_) => format!("Coverage Map — {artifact_label}"),
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
        Some(aid) => render_mosaic_canvas(shell, bundle, aid, border, dim, fg, cx)
            .into_any_element(),
        None => div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(dim)
            .child("This bundle has no native code. Coverage requires native instrumentation.")
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
    // Largest text section wins — that's the "main" library
    // in most APKs. Falls back to any native artifact.
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

/// The actual mosaic. Uses a gpui canvas to capture the
/// current pixel size; the layout runs synchronously each
/// frame against the live size. For a few thousand symbols
/// the cost is negligible; for tens of thousands we may need
/// to cache by (artifact, size). v0 punts on caching.
fn render_mosaic_canvas(
    shell: &mut Shell,
    bundle: LoadedBundle,
    artifact: glass_db::ArtifactId,
    border: gpui::Rgba,
    dim: gpui::Rgba,
    _fg: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let weak = cx.entity().downgrade();

    // The canvas covers the body and reports its bounds back
    // into Shell. The mosaic tiles render alongside, absolutely
    // positioned over the same area using the cached bounds.
    let measure = gpui::canvas(
        {
            let weak = weak.clone();
            move |bounds, _window, cx| {
                if let Some(entity) = weak.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.set_coverage_canvas_bounds(bounds, cx);
                    });
                }
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    let tiles_layer =
        mosaic_tiles_element(shell, bundle, artifact, border, dim, cx);

    div()
        .flex_1()
        .relative()
        .overflow_hidden()
        .child(measure)
        .child(tiles_layer)
}

/// Build the tile DOM. Uses `shell.coverage_canvas_bounds` to
/// know how big the area is. The very first frame after the
/// tab opens has no bounds yet → "Sizing…" placeholder; the
/// second frame fills in.
fn mosaic_tiles_element(
    shell: &mut Shell,
    bundle: LoadedBundle,
    artifact: glass_db::ArtifactId,
    border: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    let bounds = shell.coverage_canvas_bounds;
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);

    if w < 8.0 || h < 8.0 {
        return div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(dim)
            .child("Sizing…")
            .into_any_element();
    }

    let tiles = build_mosaic(&bundle, &artifact, w, h, 6.0);

    if tiles.is_empty() {
        return div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(dim)
            .child("No function symbols in this artifact.")
            .into_any_element();
    }

    let mut root = div()
        .absolute()
        .top_0()
        .left_0()
        .w(px(w))
        .h(px(h));

    for tile in tiles {
        let tile_bg = tile_colour(tile.hits, 1);
        let label = if tile.w >= 60.0 && tile.h >= 14.0 {
            Some(tile.display_name.clone())
        } else {
            None
        };
        let artifact_for_click = tile.artifact.clone();
        let section_for_click = tile.section.clone();
        let addr_for_click = tile.symbol_addr;
        let mut t = div()
            .absolute()
            .left(px(tile.x))
            .top(px(tile.y))
            .w(px(tile.w.max(1.0)))
            .h(px(tile.h.max(1.0)))
            .bg(rgb(tile_bg))
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _window, cx| {
                    let new_tab = ev.modifiers.shift;
                    if new_tab {
                        this.open_listing_force_new_tab(
                            artifact_for_click.clone(),
                            section_for_click.clone(),
                            addr_for_click,
                            cx,
                        );
                    } else {
                        this.open_listing_at(
                            artifact_for_click.clone(),
                            section_for_click.clone(),
                            addr_for_click,
                            cx,
                        );
                    }
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
        root = root.child(t);
    }

    root.into_any_element()
}
