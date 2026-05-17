//! Per-frame renderer for the CFG view.
//!
//! `render_cfg(shell, ...)` is invoked from `Shell::render_cfg` as a
//! thin delegate. Most of the body is layout planning + cull/render
//! loops over blocks and edges; mouse handlers reach back into Shell
//! through a weak entity ref.

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, App, Context, Pixels, SharedString,
};

use crate::cfg_block::{
    render_cfg_block_content, render_cfg_block_pill, CfgBlockRenderCtx, CfgBlockSummary,
    CfgLayoutPlan,
};
use crate::cfg_edge::{render_edge_arrowhead, render_edge_segment, ArrowHeadDir, EdgeSegment};
use crate::graph;
use crate::{LoadedBundle, Shell};

const CFG_WORLD_UNIT: f32 = graph::WORLD_UNIT;
const LOD_PILL_MAX: f32 = 50.;

pub fn render_cfg(
    shell: &mut Shell,
    bundle: &LoadedBundle,
    artifact: &glass_db::ArtifactId,
    entry_addr: u64,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
        shell.ensure_cfg_built(artifact, entry_addr);

        let Some(active_idx) = shell.active_tab else {
            return div().size_full().bg(panel).into_any_element();
        };
        let cfg_view = shell
            .tabs
            .get(active_idx)
            .and_then(|t| t.cfg.as_ref())
            .cloned();
        let Some(view) = cfg_view else {
            return div().size_full().bg(panel).into_any_element();
        };
        let cfg = match view.cfg.clone() {
            Some(c) => c,
            None => {
                return div()
                    .size_full()
                    .bg(panel)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(dim)
                    .child(SharedString::from(format!(
                        "No CFG for function at 0x{entry_addr:x}"
                    )))
                    .into_any_element();
            }
        };

        let zoom = view.zoom();
        let pan_x = view.pan_x();
        let pan_y = view.pan_y();

        // Look up function display name for the header.
        let func_name = bundle
            .symbol_maps
            .get(artifact)
            .and_then(|sm| sm.at(entry_addr))
            .map(|s| s.display_name.clone())
            .unwrap_or_else(|| format!("sub_{entry_addr:x}"));

        // World-to-screen converter. The world origin maps to the
        // viewport's centre; `pan_x`/`pan_y` shift the world relative
        // to that. One world unit = `CFG_WORLD_UNIT * zoom` pixels.
        let bounds = view.camera.viewport_bounds;
        let bounds_origin_x = bounds.origin.x.as_f32();
        let bounds_origin_y = bounds.origin.y.as_f32();
        let bounds_width = bounds.size.width.as_f32();
        let bounds_height = bounds.size.height.as_f32();
        let centre_x = bounds_origin_x + bounds_width / 2.;
        let centre_y = bounds_origin_y + bounds_height / 2.;
        let unit = CFG_WORLD_UNIT * zoom;
        // First-paint guard: the canvas measure hook fires during
        // this same paint, so the *current* viewport_bounds is
        // still its default (0×0) on frame 1. Disable culling in
        // that case so all blocks render — they may overflow off
        // the canvas, but the next paint (triggered by the canvas
        // hook's notify) has the real bounds and re-culls.
        let bounds_unknown = bounds_width <= 0. || bounds_height <= 0.;

        let weak = cx.entity().downgrade();

        // ---- Sizing model ------------------------------------------
        //
        // Each block's *content* is at most: optional symbol header,
        // an address row, up to 3 instructions (mnemonic + operands),
        // and either an ellipsis row or an instruction-count row.
        //
        // We size every block to fit its content snugly: width = the
        // longest line × an approximate char width, clamped between
        // MIN_W and MAX_W. Height = exactly the number of content
        // rows × a per-row world height.
        // Physical text metrics. Rounded up from gpui's rendered
        // sizes plus a couple of px of slack on each row so subpixel
        // rounding can't clip the last instruction. Under-estimating
        // here costs us a visible row; over-estimating just adds a
        // little dead space at the bottom.
        const ROW_PX: f32 = 17.;
        const ELLIPSIS_ROW_PX: f32 = 28.;
        const PADDING_PX_H: f32 = 18.;
        /// Safety margin shaved off the pixel budget before
        /// `plan_layout` accepts a layout. Belt-and-braces against
        /// subpixel rounding turning a "just fits" plan into a
        /// "clipped by 1 px" render.
        const HEIGHT_FUDGE_PX: f32 = 4.;
        // Pixel-space width metrics so we can pick a tight world
        // width per block. Courier at text_xs averages ~7 px/char.
        // PADDING_PX_W covers px_2 left/right + 2 px border with a
        // little breathing room on either side.
        const CHAR_PX: f32 = 7.;
        const PADDING_PX_W: f32 = 28.;
        const MIN_BLOCK_PX_W: f32 = 80.;
        const MAX_BLOCK_PX_W: f32 = 640.;
        // A "full" (truncated) block reserves this many *world*
        // units vertically. Translates to `FULL_BLOCK_WORLD_H × unit`
        // screen pixels, so as the user zooms in the block grows on
        // screen and more rows of text fit inside it. Zoom out and
        // the row budget shrinks down to a single line of "…
        // N instructions" + last.
        const FULL_BLOCK_WORLD_H: f32 = 0.6;
        const RANK_GAP: f32 = 0.6;
        const COL_GAP: f32 = 0.25;

        // ---- Row budget driven by pixel height -------------------
        //
        // A "full" (truncated) block has FULL_BLOCK_PX_H pixels of
        // screen height. The truncated layout renders:
        //   - optional symbol header        (ROW_PX)
        //   - `preview` instruction rows    (preview * ROW_PX)
        //   - one "… N instructions" line   (ELLIPSIS_ROW_PX, taller)
        //   - one last-instruction row      (ROW_PX)
        //   - top + bottom padding          (PADDING_PX_H)
        // and the total must fit in FULL_BLOCK_PX_H. Solving for
        // `preview` gives the budget below.
        // Per-frame screen budget for a truncated block, derived
        // from the constant *world* height: scales with zoom so
        // zooming in really does grow the block (and lets more
        // rows fit). At very small zoom the budget can drop below
        // even one row, in which case we sacrifice the ellipsis
        // line first and the last instruction last — the user must
        // always see at least one line, and preferably the last
        // instruction of the block.
        let full_block_px_h = FULL_BLOCK_WORLD_H * unit;
        // Effective budget used by `plan_layout` to decide whether
        // a candidate layout fits. The fudge ensures the rendered
        // height never ends up over the actual block height after
        // subpixel rounding.
        let budget_px_h = full_block_px_h - HEIGHT_FUDGE_PX;

        // Plan the block layout given the pixel budget. Picks the
        // most informative layout that fits.
        let plan_layout = move |b: &glass_arch_arm64::BasicBlock,
                                 has_symbol: bool|
         -> CfgLayoutPlan {
            let n = b.instructions.len();
            if n == 0 {
                return CfgLayoutPlan {
                    preview: 0,
                    show_ellipsis: false,
                    show_last: false,
                };
            }
            let sym_h = if has_symbol { ROW_PX } else { 0. };
            // The full-show layout: sym? + n × ROW_PX + padding.
            let full_h = sym_h + (n as f32) * ROW_PX + PADDING_PX_H;
            if full_h <= budget_px_h {
                return CfgLayoutPlan {
                    preview: n,
                    show_ellipsis: false,
                    show_last: false,
                };
            }
            // Try the truncated layout: maximize preview while
            // keeping (sym? + preview + ellipsis + last + padding)
            // within the budget.
            let mut best_preview: Option<usize> = None;
            for k in 0..n.saturating_sub(1) {
                let h = sym_h
                    + (k as f32) * ROW_PX
                    + ELLIPSIS_ROW_PX
                    + ROW_PX
                    + PADDING_PX_H;
                if h <= budget_px_h {
                    best_preview = Some(k);
                } else {
                    break;
                }
            }
            if let Some(preview) = best_preview {
                return CfgLayoutPlan {
                    preview,
                    show_ellipsis: true,
                    show_last: true,
                };
            }
            // Budget too small for ellipsis+last. Try ellipsis+last
            // only (no preview rows):
            //   sym? + ellipsis + last + padding
            if sym_h + ELLIPSIS_ROW_PX + ROW_PX + PADDING_PX_H <= budget_px_h {
                return CfgLayoutPlan {
                    preview: 0,
                    show_ellipsis: true,
                    show_last: true,
                };
            }
            // Tighter still: just the last instruction.
            if sym_h + ROW_PX + PADDING_PX_H <= budget_px_h {
                return CfgLayoutPlan {
                    preview: 0,
                    show_ellipsis: false,
                    show_last: true,
                };
            }
            // Smallest fit: just `… N instructions` with no
            // last-instruction row. The user knows the block has
            // content but it's about to collapse to the pill LOD.
            CfgLayoutPlan {
                preview: 0,
                show_ellipsis: true,
                show_last: false,
            }
        };

        let symbols = bundle.symbol_maps.get(artifact);
        let symbol_for_block = |b: &glass_arch_arm64::BasicBlock| -> Option<SharedString> {
            symbols
                .and_then(|sm| sm.at(b.start_addr))
                .map(|s| SharedString::from(s.display_name.clone()))
        };
        // Resolve every call's target address to a function entry +
        // display name via the artifact's symbol map. Direct calls
        // (`bl <imm>`) get a resolved name; indirect calls (`blr`)
        // have target_addr = None and are skipped.
        let resolve_call =
            |addr: u64| -> Option<(u64, SharedString)> {
                let sym = symbols.and_then(|sm| sm.covering(addr))?;
                Some((sym.address, SharedString::from(sym.display_name.clone())))
            };
        let summaries: Vec<CfgBlockSummary> = cfg
            .blocks
            .iter()
            .map(|b| {
                let mut calls = std::collections::HashMap::new();
                for c in &b.calls {
                    if let Some(tgt) = c.target_addr {
                        if let Some(resolved) = resolve_call(tgt) {
                            calls.insert(c.site_addr, resolved);
                        }
                    }
                }
                CfgBlockSummary {
                    symbol: symbol_for_block(b),
                    calls,
                }
            })
            .collect();

        // Per-block size (world units). Width is sized from the
        // longest displayed line in screen pixels, then converted to
        // world units via `unit` so it stays visually constant
        // across zoom — at higher zoom the block doesn't waste
        // space on wider boxes. Height is exact (no dead space) and
        // accounts for the ellipsis row's larger height when
        // truncating.
        let block_size = |block: &glass_arch_arm64::BasicBlock,
                          summary: &CfgBlockSummary,
                          plan: CfgLayoutPlan|
         -> (f32, f32) {
            const ADDR_COL: usize = 16 + 1; // "0123456789abcdef "
            let mut longest = 0usize;
            if let Some(name) = summary.symbol.as_ref() {
                longest = longest.max(name.len() + 1); // ":" suffix
            }
            let insn_line_len = |insn: &glass_arch_arm64::InstructionEntry| -> usize {
                // When the operand is a call whose target resolved
                // to a symbol, we render the symbol name in place of
                // the raw operand text — size for that length so
                // long callee names don't get truncated.
                let operand_len = match summary.calls.get(&insn.address) {
                    Some((_, name)) => name.len(),
                    None => insn.operands.len(),
                };
                ADDR_COL
                    + insn.mnemonic.len()
                    + if operand_len == 0 { 0 } else { 1 + operand_len }
            };
            let has_sym = summary.symbol.is_some();
            let n = block.instructions.len();

            for insn in block.instructions.iter().take(plan.preview) {
                longest = longest.max(insn_line_len(insn));
            }
            if plan.show_ellipsis {
                let skipped = n
                    .saturating_sub(plan.preview)
                    .saturating_sub(if plan.show_last { 1 } else { 0 });
                let footer_len = 2 + format!("{skipped} instructions").len();
                longest = longest.max(footer_len);
            }
            if plan.show_last {
                if let Some(last) = block.instructions.last() {
                    longest = longest.max(insn_line_len(last));
                }
            }
            if n == 0 {
                longest = longest.max("(empty)".len());
            }
            // Width: longest-line pixels → world units.
            let w_px = ((longest as f32) * CHAR_PX + PADDING_PX_W)
                .clamp(MIN_BLOCK_PX_W, MAX_BLOCK_PX_W);
            let w = w_px / unit;

            // Height: sum of exactly what we'll render. Each row is
            // ROW_PX except the ellipsis (ELLIPSIS_ROW_PX).
            let mut content_px = PADDING_PX_H;
            if has_sym {
                content_px += ROW_PX;
            }
            content_px += (plan.preview as f32) * ROW_PX;
            if plan.show_ellipsis {
                content_px += ELLIPSIS_ROW_PX;
            }
            if plan.show_last {
                content_px += ROW_PX;
            }
            if n == 0 {
                content_px += ROW_PX; // (empty)
            }
            let h = content_px.max(ROW_PX + PADDING_PX_H) / unit;
            (w, h)
        };

        // Plan + size each block once. The plan is reused at render
        // time so layout sizing and rendering stay in lockstep.
        let plans: Vec<CfgLayoutPlan> = cfg
            .blocks
            .iter()
            .zip(summaries.iter())
            .map(|(b, s)| plan_layout(b, s.symbol.is_some()))
            .collect();
        let mut sizes: Vec<(f32, f32)> = Vec::with_capacity(cfg.blocks.len());
        for ((block, summary), plan) in
            cfg.blocks.iter().zip(summaries.iter()).zip(plans.iter())
        {
            sizes.push(block_size(block, summary, *plan));
        }

        // Group block indices by rank, preserving discovery order.
        let mut by_rank: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, layout) in cfg.layout.iter().enumerate() {
            by_rank.entry(layout.rank).or_default().push(i);
        }

        // Place each block using the CFG's barycenter-tuned x as a
        // hint. Within a rank we sort by hinted x, scale the hints
        // so they're proportional to block widths, then enforce
        // non-overlap (left-to-right) with COL_GAP between borders.
        // Centre each rank's pack on x = 0.
        let mut world_pos: Vec<(f32, f32)> = vec![(0., 0.); cfg.blocks.len()];
        let mut cursor_y = 0.0_f32;
        for (_rank, indices) in &by_rank {
            // Sort the rank by the layout's hinted x so the order
            // reflects the barycenter pass (relative parent/child
            // alignment), not just discovery order.
            let mut ordered: Vec<usize> = indices.clone();
            ordered.sort_by(|&a, &b| {
                cfg.layout[a]
                    .x
                    .partial_cmp(&cfg.layout[b].x)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(&b))
            });
            // Scale hinted x's to honour block widths: each block's
            // left edge starts at hint_x scaled to "average block
            // width + COL_GAP" so blocks roughly align with their
            // ideal positions but don't overlap.
            let max_h = ordered
                .iter()
                .map(|&i| sizes[i].1)
                .fold(0.0_f32, f32::max);
            // First pass: pick a left x for each block from the
            // hinted positions, scaled.
            let scale = ordered
                .iter()
                .map(|&i| sizes[i].0)
                .fold(0.0_f32, f32::max)
                .max(1.0)
                + COL_GAP;
            // Map hint_x ranges to actual placement: place each
            // block centred on hint_x * scale, then enforce min-gap.
            let mut placement: Vec<(f32, f32, usize)> = ordered
                .iter()
                .map(|&i| {
                    let w = sizes[i].0;
                    let hint = cfg.layout[i].x;
                    let left = hint * scale - w / 2.;
                    (left, w, i)
                })
                .collect();
            // Walk left-to-right: each block's left must be at
            // least previous.left + previous.w + COL_GAP.
            for k in 1..placement.len() {
                let (prev_left, prev_w, _) = placement[k - 1];
                let min_left = prev_left + prev_w + COL_GAP;
                if placement[k].0 < min_left {
                    placement[k].0 = min_left;
                }
            }
            // Centre the rank.
            let (first_left, _, _) = placement[0];
            let (last_left, last_w, _) = placement[placement.len() - 1];
            let total_extent = last_left + last_w - first_left;
            let shift = -first_left - total_extent / 2.;
            for &(left, _, i) in &placement {
                world_pos[i] = (left + shift, cursor_y);
            }
            cursor_y += max_h + RANK_GAP;
        }

        // Per-block screen rect, computed once, reused for blocks +
        // edges. Indexed by block id.
        struct ScreenRect {
            // In *local* (scene) pixel coordinates.
            x: f32,
            y: f32,
            w: f32,
            h: f32,
        }
        let mut rects: Vec<ScreenRect> = Vec::with_capacity(cfg.blocks.len());
        for (i, _block) in cfg.blocks.iter().enumerate() {
            let (world_x, world_y) = world_pos[i];
            let (w_world, h_world) = sizes[i];
            let screen_x_px = centre_x + (world_x - pan_x) * unit;
            let screen_y_px = centre_y + (world_y - pan_y) * unit;
            let screen_w_px = w_world * unit;
            let screen_h_px = h_world * unit;
            rects.push(ScreenRect {
                x: screen_x_px - bounds_origin_x,
                y: screen_y_px - bounds_origin_y,
                w: screen_w_px,
                h: screen_h_px,
            });
        }

        // Build the absolute-positioned scene.
        let mut scene = div()
            .id("cfg-scene")
            .absolute()
            .top_0()
            .left_0()
            .size_full();

        // ---- Edge routing prep -------------------------------------
        //
        // Pre-compute everything the router needs in screen-pixel
        // space. The router is built around two ideas:
        //
        // 1. *Rank-gap bands.* Between consecutive ranks lies an
        //    empty horizontal band (the RANK_GAP we inserted at
        //    placement). Horizontal edge segments live inside those
        //    bands so they never cross a block.
        //
        // 2. *Free vertical lanes.* A vertical x is "clear" across
        //    a range of ranks if no block in those ranks covers
        //    that x. For an edge from rank `R_s` to rank `R_t`, we
        //    walk candidate x's outward from the source/target
        //    columns until we find one clear of every block in
        //    ranks (R_s+1 .. R_t-1) and approach-side blocks in
        //    R_s/R_t. That's where the long vertical leg goes.
        let bounds_w = bounds_width;
        let bounds_h = bounds_height;
        let _ = (bounds_w, bounds_h);

        // Per-block fan-in/fan-out counts + edge ordering. We sort
        // each block's outgoing edges by the *target x* (so edges
        // exit the source in the same left-to-right order their
        // targets sit on screen) and incoming edges by *source x*.
        // This eliminates pointless crossings where edges with
        // targets on the right currently exit from a left-side slot.
        let mut in_edges: Vec<Vec<usize>> = vec![Vec::new(); cfg.blocks.len()];
        let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); cfg.blocks.len()];
        for (ei, edge) in cfg.edges.iter().enumerate() {
            if edge.to.0 < in_edges.len() {
                in_edges[edge.to.0].push(ei);
            }
            if edge.from.0 < out_edges.len() {
                out_edges[edge.from.0].push(ei);
            }
        }
        // For each block, sort outgoing by target x; incoming by
        // source x. Build per-edge slot index lookups.
        let mut out_slot: Vec<usize> = vec![0; cfg.edges.len()];
        let mut in_slot: Vec<usize> = vec![0; cfg.edges.len()];
        for (bi, eids) in out_edges.iter_mut().enumerate() {
            eids.sort_by(|&a, &b| {
                let xa = rects
                    .get(cfg.edges[a].to.0)
                    .map(|r| r.x + r.w / 2.)
                    .unwrap_or(0.);
                let xb = rects
                    .get(cfg.edges[b].to.0)
                    .map(|r| r.x + r.w / 2.)
                    .unwrap_or(0.);
                xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal)
            });
            for (slot, &ei) in eids.iter().enumerate() {
                out_slot[ei] = slot;
            }
            let _ = bi;
        }
        for (bi, eids) in in_edges.iter_mut().enumerate() {
            eids.sort_by(|&a, &b| {
                let xa = rects
                    .get(cfg.edges[a].from.0)
                    .map(|r| r.x + r.w / 2.)
                    .unwrap_or(0.);
                let xb = rects
                    .get(cfg.edges[b].from.0)
                    .map(|r| r.x + r.w / 2.)
                    .unwrap_or(0.);
                xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal)
            });
            for (slot, &ei) in eids.iter().enumerate() {
                in_slot[ei] = slot;
            }
            let _ = bi;
        }
        let in_total: Vec<usize> = in_edges.iter().map(|v| v.len()).collect();
        let out_total: Vec<usize> = out_edges.iter().map(|v| v.len()).collect();

        // For each rank: the y at the bottom of its tallest block,
        // the y at the top of the next rank below, and the list of
        // (x_left, x_right) intervals occupied by blocks (sorted).
        struct RankGeom {
            bottom_y: f32,
            next_top_y: f32,
            intervals: Vec<(f32, f32)>,
        }
        let rank_of_block: Vec<usize> = cfg.layout.iter().map(|l| l.rank).collect();
        let mut rank_geom: std::collections::BTreeMap<usize, RankGeom> =
            std::collections::BTreeMap::new();
        for (rank, indices) in &by_rank {
            let bottom_y = indices
                .iter()
                .map(|&i| rects[i].y + rects[i].h)
                .fold(f32::MIN, f32::max);
            let mut intervals: Vec<(f32, f32)> = indices
                .iter()
                .map(|&i| (rects[i].x, rects[i].x + rects[i].w))
                .collect();
            intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            rank_geom.insert(
                *rank,
                RankGeom {
                    bottom_y,
                    next_top_y: bottom_y, // filled in below
                    intervals,
                },
            );
        }
        let ranks_sorted: Vec<usize> = rank_geom.keys().copied().collect();
        for w in ranks_sorted.windows(2) {
            let upper = w[0];
            let lower = w[1];
            let next_top = by_rank[&lower]
                .iter()
                .map(|&i| rects[i].y)
                .fold(f32::MAX, f32::min);
            if let Some(entry) = rank_geom.get_mut(&upper) {
                entry.next_top_y = next_top;
            }
        }
        // Bounds of the canvas content — used as fallback "outside"
        // lanes when no internal channel is free.
        let scene_left = rects
            .iter()
            .map(|r| r.x)
            .fold(f32::MAX, f32::min)
            - 24.;
        let scene_right = rects
            .iter()
            .map(|r| r.x + r.w)
            .fold(f32::MIN, f32::max)
            + 24.;

        // Per-rank-gap horizontal-lane allocator. Each entry maps
        // `rank` → list of (allocated_y) for edges using the gap
        // below it. We assign each edge its own y within the gap.
        let mut h_lanes: std::collections::BTreeMap<usize, Vec<f32>> =
            std::collections::BTreeMap::new();
        // Per-vertical-lane allocator. Vertical lanes are picked by
        // x; multiple edges in the same x channel get stacked
        // horizontally with a small offset so they don't overlap.
        let mut v_lane_count: std::collections::HashMap<i32, usize> =
            std::collections::HashMap::new();
        // Returns a clear vertical x between source and target,
        // searching outward from `prefer` (typically the average of
        // source and target x). Skips any x that crosses a block in
        // ranks strictly between the source and target.
        fn pick_vertical_lane(
            prefer: f32,
            rank_lo: usize,
            rank_hi: usize,
            rank_geom: &std::collections::BTreeMap<usize, RankGeom>,
            scene_left: f32,
            scene_right: f32,
        ) -> f32 {
            // A vertical at x is clear across ranks [lo+1, hi-1]
            // (the intermediate ranks the edge crosses) when no
            // block in any of those ranks contains x. The source
            // and target ranks themselves are exited / entered via
            // the rank-gap turns, so we don't need to clear them.
            let blocks: &Vec<(f32, f32)> = &{
                let mut out: Vec<(f32, f32)> = Vec::new();
                for r in (rank_lo.min(rank_hi))..=(rank_lo.max(rank_hi)) {
                    if r == rank_lo || r == rank_hi {
                        continue;
                    }
                    if let Some(g) = rank_geom.get(&r) {
                        out.extend(g.intervals.iter().copied());
                    }
                }
                out
            };
            let clear = |x: f32| -> bool {
                let margin = 4.;
                !blocks
                    .iter()
                    .any(|&(l, r)| x >= l - margin && x <= r + margin)
            };
            if clear(prefer) {
                return prefer;
            }
            // Walk outward in expanding steps until we find a free x
            // or hit the canvas bounds.
            let step = 12.;
            for k in 1..200 {
                let dx = step * k as f32;
                let left = prefer - dx;
                if left >= scene_left && clear(left) {
                    return left;
                }
                let right = prefer + dx;
                if right <= scene_right && clear(right) {
                    return right;
                }
                if left < scene_left && right > scene_right {
                    break;
                }
            }
            // Nothing found — fall back to the side highway.
            if (prefer - scene_left).abs() < (prefer - scene_right).abs() {
                scene_left
            } else {
                scene_right
            }
        }

        // ---- Edges first so blocks render on top of them. ----------
        for (edge_idx, edge) in cfg.edges.iter().enumerate() {
            let Some(src) = rects.get(edge.from.0) else { continue };
            let Some(dst) = rects.get(edge.to.0) else { continue };
            let from_idx = edge.from.0;
            let to_idx = edge.to.0;

            // Fan-in / fan-out attach fractions, ordered by the
            // x position of the *other* end. So edges to the right
            // exit through the right portion of the source's bottom
            // edge; edges from the left enter through the left
            // portion of the target's top edge.
            let out_n = out_total[from_idx].max(1);
            let in_n = in_total[to_idx].max(1);
            let out_frac =
                (out_slot[edge_idx] + 1) as f32 / (out_n + 1) as f32;
            let in_frac =
                (in_slot[edge_idx] + 1) as f32 / (in_n + 1) as f32;
            let sx = src.x + src.w * out_frac;
            let sy = src.y + src.h;
            let tx = dst.x + dst.w * in_frac;
            let ty = dst.y;

            let both_off = !bounds_unknown
                && ((sx < 0. && tx < 0.)
                    || (sx > bounds_w && tx > bounds_w)
                    || (sy < 0. && ty < 0.)
                    || (sy > bounds_h && ty > bounds_h));
            if both_off {
                continue;
            }
            let dotted = matches!(
                edge.kind,
                glass_arch_arm64::BlockEdgeKind::TakenConditional
                    | glass_arch_arm64::BlockEdgeKind::NotTakenConditional,
            );

            let from_rank = rank_of_block.get(from_idx).copied().unwrap_or(0);
            let to_rank = rank_of_block.get(to_idx).copied().unwrap_or(0);

            // Horizontal lane y for the source's rank gap. Each
            // edge in the same gap stacks vertically by 4 px.
            let gap_top = rank_geom
                .get(&from_rank)
                .map(|g| g.bottom_y)
                .unwrap_or(sy);
            let gap_bottom = rank_geom
                .get(&from_rank)
                .map(|g| g.next_top_y)
                .unwrap_or(sy + 24.);
            let gap_mid = (gap_top + gap_bottom) / 2.;
            let lanes = h_lanes.entry(from_rank).or_default();
            let lane_idx = lanes.len();
            // Distribute lanes around the gap midline.
            let lane_step = 5.;
            let lane_y = gap_mid + ((lane_idx as f32 / 2.).ceil() as f32)
                * lane_step
                * if lane_idx % 2 == 0 { 1. } else { -1. };
            // Clamp inside the gap.
            let half = ((gap_bottom - gap_top).abs() / 2. - 4.).max(0.);
            let lane_y = lane_y.clamp(gap_mid - half, gap_mid + half);
            lanes.push(lane_y);

            // Routing modes:
            //   - Forward adjacent rank: 3-segment route via rank-gap.
            //   - Forward multi-rank: 5-segment route via a clear
            //     vertical channel.
            //   - Back-edge (to_rank <= from_rank): exit source side,
            //     run up the side highway, enter target side.
            let single_rank_forward = to_rank == from_rank + 1;
            let is_back_edge = to_rank <= from_rank;
            let segments: Vec<EdgeSegment>;
            let arrow_pos: (f32, f32, ArrowHeadDir);

            // Pixels the final line segment is shortened by so the
            // arrowhead's wedge body isn't painted over by the line.
            const ARROW_TRIM_PX: f32 = 7.;

            if single_rank_forward {
                // Simple 3-segment route via the rank-gap lane.
                let final_y_top = lane_y.min(ty);
                let final_y_len = (ty - lane_y).abs() - ARROW_TRIM_PX;
                segments = vec![
                    EdgeSegment {
                        x: sx,
                        y: sy.min(lane_y),
                        length: (lane_y - sy).abs(),
                        horizontal: false,
                    },
                    EdgeSegment {
                        x: sx.min(tx),
                        y: lane_y,
                        length: (tx - sx).abs(),
                        horizontal: true,
                    },
                    EdgeSegment {
                        x: tx,
                        y: final_y_top,
                        length: final_y_len.max(0.),
                        horizontal: false,
                    },
                ];
                arrow_pos = (tx, ty, ArrowHeadDir::Down);
            } else if is_back_edge {
                // Back-edge: route via a vertical highway clear of
                // every block, entering the target's side. Pick the
                // highway side that gives the cleanest path — the
                // side furthest from the source/target column range
                // so we never cross either block.
                let exit_y = src.y + src.h * out_frac;
                let entry_y = dst.y + dst.h * in_frac;
                // Try both sides; pick whichever yields a clear
                // vertical lane closer to the source.
                let right_prefer = src.x.max(dst.x + dst.w) + 24.;
                let left_prefer = src.x.min(dst.x) - 24.;
                let right_lane = pick_vertical_lane(
                    right_prefer,
                    from_rank,
                    to_rank,
                    &rank_geom,
                    scene_left,
                    scene_right,
                );
                let left_lane = pick_vertical_lane(
                    left_prefer,
                    from_rank,
                    to_rank,
                    &rank_geom,
                    scene_left,
                    scene_right,
                );
                // Choose whichever side gives a shorter total
                // horizontal travel.
                let right_cost = (right_lane - (src.x + src.w)).abs()
                    + (right_lane - (dst.x + dst.w)).abs();
                let left_cost = (left_lane - src.x).abs()
                    + (left_lane - dst.x).abs();
                let use_right = right_cost <= left_cost;
                let v_lane_x = if use_right { right_lane } else { left_lane };
                // Exit and entry sides face the highway.
                let exit_side_x = if use_right { src.x + src.w } else { src.x };
                let entry_side_x = if use_right { dst.x + dst.w } else { dst.x };
                let key = (v_lane_x / 6.).round() as i32;
                let n = v_lane_count.entry(key).or_insert(0);
                let v_offset = (*n as f32)
                    * 4.
                    * if use_right { 1. } else { -1. };
                *n += 1;
                let v_x = v_lane_x + v_offset;

                // Trim the final horizontal segment so the
                // arrowhead's wedge body isn't covered by the line.
                let (h3_x, h3_len) = if use_right {
                    // Line comes from the right, ends at the target's
                    // right side. Stop ARROW_TRIM_PX away.
                    let stop_x = entry_side_x + ARROW_TRIM_PX;
                    (
                        stop_x.min(v_x),
                        ((v_x - stop_x).abs() - 0.).max(0.),
                    )
                } else {
                    // Line comes from the left, ends at the target's
                    // left side. Stop ARROW_TRIM_PX away.
                    let stop_x = entry_side_x - ARROW_TRIM_PX;
                    (
                        v_x.min(stop_x),
                        ((stop_x - v_x).abs() - 0.).max(0.),
                    )
                };
                segments = vec![
                    // 1: horizontal from source side to highway.
                    EdgeSegment {
                        x: exit_side_x.min(v_x),
                        y: exit_y,
                        length: (v_x - exit_side_x).abs(),
                        horizontal: true,
                    },
                    // 2: vertical at v_x from exit_y to entry_y.
                    EdgeSegment {
                        x: v_x,
                        y: exit_y.min(entry_y),
                        length: (entry_y - exit_y).abs(),
                        horizontal: false,
                    },
                    // 3: horizontal from highway to target side
                    //    (stops short of the arrowhead).
                    EdgeSegment {
                        x: h3_x,
                        y: entry_y,
                        length: h3_len,
                        horizontal: true,
                    },
                ];
                // Arrow enters target's side. If we exited and
                // entered on the right, the line approaches the
                // target from its right and the arrow points Left.
                // If on the left, the arrow points Right.
                arrow_pos = (
                    entry_side_x,
                    entry_y,
                    if use_right {
                        ArrowHeadDir::Left
                    } else {
                        ArrowHeadDir::Right
                    },
                );
            } else {
                // Forward multi-rank. Pick a vertical lane outside
                // any intermediate block. Bias toward the average of
                // source and target x so edges don't all pile on
                // the same side.
                let prefer = (sx + tx) / 2.;
                let v_lane_x = pick_vertical_lane(
                    prefer,
                    from_rank,
                    to_rank,
                    &rank_geom,
                    scene_left,
                    scene_right,
                );
                let key = (v_lane_x / 6.).round() as i32;
                let n = v_lane_count.entry(key).or_insert(0);
                let v_offset = (*n as f32) * 4.;
                *n += 1;
                let v_x = v_lane_x + v_offset;

                let target_gap_top = rank_geom
                    .iter()
                    .filter(|(r, _)| **r + 1 == to_rank)
                    .map(|(_, g)| g.bottom_y)
                    .next()
                    .unwrap_or(ty - 24.);
                let target_gap_bottom = rank_geom
                    .iter()
                    .filter(|(r, _)| **r + 1 == to_rank)
                    .map(|(_, g)| g.next_top_y)
                    .next()
                    .unwrap_or(ty);
                let approach_y = (target_gap_top + target_gap_bottom) / 2.;

                let final_y_top = approach_y.min(ty);
                let final_y_len = (ty - approach_y).abs() - ARROW_TRIM_PX;
                segments = vec![
                    EdgeSegment {
                        x: sx,
                        y: sy.min(lane_y),
                        length: (lane_y - sy).abs(),
                        horizontal: false,
                    },
                    EdgeSegment {
                        x: sx.min(v_x),
                        y: lane_y,
                        length: (v_x - sx).abs(),
                        horizontal: true,
                    },
                    EdgeSegment {
                        x: v_x,
                        y: lane_y.min(approach_y),
                        length: (approach_y - lane_y).abs(),
                        horizontal: false,
                    },
                    EdgeSegment {
                        x: v_x.min(tx),
                        y: approach_y,
                        length: (tx - v_x).abs(),
                        horizontal: true,
                    },
                    EdgeSegment {
                        x: tx,
                        y: final_y_top,
                        length: final_y_len.max(0.),
                        horizontal: false,
                    },
                ];
                arrow_pos = (tx, ty, ArrowHeadDir::Down);
            }
            for seg in segments {
                scene = scene.child(render_edge_segment(seg, dotted));
            }
            scene = scene.child(render_edge_arrowhead(
                arrow_pos.0,
                arrow_pos.1,
                arrow_pos.2,
            ));
        }

        // ---- Blocks ------------------------------------------------
        for (i, block) in cfg.blocks.iter().enumerate() {
            let rect = &rects[i];
            // Cull off-viewport blocks. Skip culling on the first
            // paint when the viewport bounds aren't known yet —
            // otherwise only the block at the origin would render
            // and the user would have to pan to trigger a refresh.
            let off_screen = !bounds_unknown
                && (rect.x + rect.w < 0.
                    || rect.x > bounds_width
                    || rect.y + rect.h < 0.
                    || rect.y > bounds_height);
            if off_screen {
                continue;
            }
            let summary = &summaries[i];
            // LOD selection based on on-screen width.
            let block_el = if rect.w < LOD_PILL_MAX {
                render_cfg_block_pill(block, summary, dim)
            } else {
                let block_ctx = CfgBlockRenderCtx {
                    shell: weak.clone(),
                    artifact: artifact.clone(),
                    block_idx: i,
                };
                render_cfg_block_content(block, summary, plans[i], Some(&block_ctx))
            };
            let click_weak = weak.clone();
            let click_artifact = artifact.clone();
            let block_addr = block.start_addr;
            scene = scene.child(
                div()
                    .id(("cfg-block", i))
                    .absolute()
                    .left(px(rect.x))
                    .top(px(rect.y))
                    .w(px(rect.w))
                    .h(px(rect.h))
                    .cursor_pointer()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_ev, _w, cx: &mut App| {
                            if let Some(entity) = click_weak.upgrade() {
                                let artifact = click_artifact.clone();
                                cx.update_entity(&entity, |shell, cx| {
                                    shell.open_listing_at_addr(
                                        artifact, block_addr, cx,
                                    );
                                });
                            }
                        },
                    )
                    .child(block_el),
            );
        }

        // Capture viewport bounds each frame so pan/zoom math has
        // current values.
        let bounds_weak = weak.clone();
        let measure = gpui::canvas(
            move |bounds, _window, cx| {
                if let Some(entity) = bounds_weak.upgrade() {
                    cx.update_entity(&entity, |shell, _cx| {
                        if let Some(idx) = shell.active_tab {
                            if let Some(tab) = shell.tabs.get_mut(idx) {
                                if let Some(view) = tab.cfg.as_mut() {
                                    view.camera.viewport_bounds = bounds;
                                }
                            }
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

        let header = div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .border_b_1()
            .border_color(border)
            .text_sm()
            .text_color(fg)
            .font_family("Menlo")
            .child(SharedString::from(func_name))
            .child(
                div()
                    .text_color(dim)
                    .child(SharedString::from(format!(
                        "{} blocks · {} edges · zoom {:.0}%",
                        cfg.blocks.len(),
                        cfg.edges.len(),
                        zoom * 100.,
                    ))),
            );

        // Event handlers on the canvas surface.
        let zoom_weak = weak.clone();
        let pan_weak = weak.clone();
        let drag_weak = weak.clone();
        let drag_move_weak = weak.clone();
        let drag_end_weak = weak.clone();

        let canvas_body = div()
            .id("cfg-canvas")
            .flex_1()
            .relative()
            .overflow_hidden()
            .bg(panel)
            .child(measure)
            .child(scene)
            // Trackpad / mouse-wheel: cmd or ctrl held = zoom around
            // cursor; otherwise pan.
            .on_scroll_wheel(move |ev: &gpui::ScrollWheelEvent, _w, cx| {
                if let Some(entity) = zoom_weak.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        let delta = ev.delta.pixel_delta(px(20.));
                        // Zoom on Shift / Cmd / Ctrl + scroll. Plain
                        // scroll pans (trackpad gesture or wheel).
                        if ev.modifiers.shift
                            || ev.modifiers.platform
                            || ev.modifiers.control
                        {
                            // Shift+wheel turns vertical scroll into
                            // horizontal on some mice, so fall back
                            // to whichever axis carries the input.
                            let raw = if delta.y.as_f32().abs() > 0. {
                                delta.y.as_f32()
                            } else {
                                delta.x.as_f32()
                            };
                            shell.cfg_zoom_by(ev.position, raw, cx);
                        } else {
                            shell.cfg_pan_by(delta.x.as_f32(), delta.y.as_f32(), cx);
                        }
                    });
                }
                let _ = pan_weak;
            })
            // Mouse drag pan (middle button or left+space; for v1 we
            // accept any-button drag).
            .on_mouse_down(
                gpui::MouseButton::Middle,
                move |ev: &gpui::MouseDownEvent, _w, cx| {
                    if let Some(entity) = drag_weak.upgrade() {
                        let pos = ev.position;
                        cx.update_entity(&entity, |shell, _cx| {
                            shell.cfg_drag_start(pos);
                        });
                    }
                },
            )
            .on_mouse_move(move |ev: &gpui::MouseMoveEvent, _w, cx| {
                if let Some(entity) = drag_move_weak.upgrade() {
                    let pos = ev.position;
                    cx.update_entity(&entity, |shell, cx| {
                        shell.cfg_drag_move(pos, cx);
                    });
                }
            })
            .on_mouse_up(
                gpui::MouseButton::Middle,
                move |_ev: &gpui::MouseUpEvent, _w, cx| {
                    if let Some(entity) = drag_end_weak.upgrade() {
                        cx.update_entity(&entity, |shell, _cx| {
                            shell.cfg_drag_end();
                        });
                    }
                },
            );

        div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(panel)
            .child(header)
            .child(canvas_body)
            .into_any_element()
    }
