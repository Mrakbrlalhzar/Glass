//! Per-frame renderer for the CFG view.
//!
//! Thin shell over `graph_canvas::render_graph_canvas`. The CFG-
//! specific bits are:
//!
//!   * Build a `GraphScene` from the lazily-computed `FunctionCfg`
//!     each frame. Sizes are planned from the current zoom so blocks
//!     fit their content snugly at every LOD; positions are seeded
//!     with the arch crate's barycenter-tuned x as the `x_hint`.
//!   * Per-node content render: pill at low LOD, full-content (symbol
//!     header + addresses + truncated instruction list) at higher
//!     LOD. Calls inside the block resolve to symbol names and are
//!     click-to-jump (handled inside `cfg_block::render_cfg_block_content`).
//!   * Left-click on a block opens the block's first address in the
//!     linear listing.
//!   * Camera + drag wired to the `cfg` tab state.

use std::sync::Arc;

use gpui::{div, prelude::*, AnyElement, Context, SharedString};

use crate::cfg_block::{
    render_cfg_block_content, render_cfg_block_pill, CfgBlockRenderCtx, CfgBlockSummary,
    CfgLayoutPlan,
};
use crate::graph::{
    self, EdgeKind, EdgeStyle, GraphScene, NodeHints, NodeId, NodeRect, NodeTags,
};
use crate::graph_canvas::{
    render_graph_canvas, CameraHooks, NodeClickFn, NodeContentFn, NodeRightClickFn,
};
use crate::{LoadedBundle, Shell};

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
) -> AnyElement {
    shell.ensure_cfg_built(artifact, entry_addr);

    let Some(active_idx) = shell.active_tab else {
        return div().size_full().bg(panel).into_any_element();
    };
    let view = shell
        .tabs
        .get(active_idx)
        .and_then(|t| t.cfg.as_ref())
        .cloned();
    let Some(view) = view else {
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
    let unit = graph::WORLD_UNIT * zoom;

    // Resolve symbols for the function header + per-block symbol
    // labels + per-call site jump targets.
    let func_name = bundle
        .symbol_maps
        .get(artifact)
        .and_then(|sm| sm.at(entry_addr))
        .map(|s| s.display_name.clone())
        .unwrap_or_else(|| format!("sub_{entry_addr:x}"));

    let symbols = bundle.symbol_maps.get(artifact);
    let symbol_for_block =
        |b: &glass_arch_arm64::BasicBlock| -> Option<SharedString> {
            symbols
                .and_then(|sm| sm.at(b.start_addr))
                .map(|s| SharedString::from(s.display_name.clone()))
        };
    let resolve_call = |addr: u64| -> Option<(u64, SharedString)> {
        let sym = symbols.and_then(|sm| sm.covering(addr))?;
        Some((sym.address, SharedString::from(sym.display_name.clone())))
    };
    let summaries: Arc<Vec<CfgBlockSummary>> = Arc::new(
        cfg.blocks
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
                CfgBlockSummary { symbol: symbol_for_block(b), calls }
            })
            .collect(),
    );

    // Per-block layout planning + sizing. Sizes are returned in
    // *pixels* and fed to the shared layout via `NodeHints.size_px`,
    // which scales spacing in step.
    let plans = plan_blocks(&cfg, &summaries, unit);
    let sizes_px: Vec<(f32, f32)> = cfg
        .blocks
        .iter()
        .zip(summaries.iter())
        .zip(plans.iter())
        .map(|((b, s), p)| size_block_px(b, s, *p, unit))
        .collect();

    // Build the GraphScene. Each block becomes a node tagged with
    // its rank and the arch crate's barycenter-tuned x as `x_hint`.
    // Edges convert directly from `BlockEdgeKind`.
    let mut scene = GraphScene::default();
    for (i, block) in cfg.blocks.iter().enumerate() {
        let layout = cfg.layout.get(i);
        let rank = layout.map(|l| l.rank);
        let x_hint = layout.map(|l| l.x);
        scene.add_node(
            SharedString::from(format!("0x{:x}", block.start_addr)),
            NodeHints {
                size_px: sizes_px[i],
                rank,
                x_hint,
            },
            NodeTags {
                is_entry: block.start_addr == cfg.entry_addr,
                is_exit: block.exits_function,
            },
        );
    }
    for edge in &cfg.edges {
        if edge.from.0 >= scene.nodes.len() || edge.to.0 >= scene.nodes.len() {
            continue;
        }
        let style = if matches!(
            edge.kind,
            glass_arch_arm64::BlockEdgeKind::TakenConditional
                | glass_arch_arm64::BlockEdgeKind::NotTakenConditional,
        ) {
            EdgeStyle::Dotted
        } else {
            EdgeStyle::Solid
        };
        scene.add_edge(
            NodeId(edge.from.0),
            NodeId(edge.to.0),
            style,
            EdgeKind::ControlFlow,
        );
    }
    graph::layout_scene(&mut scene);

    let plans_arc: Arc<Vec<CfgLayoutPlan>> = Arc::new(plans);
    let blocks_arc: Arc<Vec<glass_arch_arm64::BasicBlock>> = Arc::new(cfg.blocks.clone());
    let artifact_arc = artifact.clone();
    let dim_for_content = dim;

    let content: NodeContentFn = {
        let plans = plans_arc.clone();
        let summaries = summaries.clone();
        let blocks = blocks_arc.clone();
        let artifact = artifact_arc.clone();
        Box::new(move |nid: NodeId, rect: NodeRect, weak| {
            let i = nid.0;
            let Some(block) = blocks.get(i) else {
                return div().into_any_element();
            };
            let summary = &summaries[i];
            if rect.w < LOD_PILL_MAX {
                render_cfg_block_pill(block, summary, dim_for_content)
            } else {
                let ctx = CfgBlockRenderCtx {
                    shell: weak.clone(),
                    artifact: artifact.clone(),
                    block_idx: i,
                };
                render_cfg_block_content(block, summary, plans[i], Some(&ctx))
            }
        })
    };

    let node_click: Option<NodeClickFn> = Some({
        let blocks = blocks_arc.clone();
        let artifact = artifact_arc.clone();
        Box::new(
            move |shell: &mut Shell,
                  nid: NodeId,
                  mods: gpui::Modifiers,
                  cx: &mut Context<Shell>| {
                let Some(block) = blocks.get(nid.0) else { return };
                if mods.shift {
                    // Shift+click → force a fresh Listing tab so the
                    // user can compare the block address against the
                    // current view.
                    let bundle = match shell.bundle().cloned() {
                        Some(b) => b,
                        None => return,
                    };
                    if let Some(section) =
                        bundle.text_section_for_addr(&artifact, block.start_addr)
                    {
                        shell.open_listing_force_new_tab(
                            artifact.clone(),
                            section.to_string(),
                            block.start_addr,
                            cx,
                        );
                    }
                } else {
                    shell.open_listing_at_addr(artifact.clone(), block.start_addr, cx);
                }
            },
        )
    });

    // Right-click on a block → open the link context menu with
    // Follow / Follow in new tab + Callers of function. Uses the
    // block's start_addr as both the navigation target and the
    // xref query target.
    let node_right_click: Option<NodeRightClickFn> = Some({
        let blocks = blocks_arc.clone();
        let artifact = artifact_arc.clone();
        Box::new(
            move |shell: &mut Shell,
                  nid: NodeId,
                  pos: gpui::Point<gpui::Pixels>,
                  cx: &mut Context<Shell>| {
                let Some(block) = blocks.get(nid.0) else { return };
                let bundle = match shell.bundle().cloned() {
                    Some(b) => b,
                    None => return,
                };
                let Some(section) =
                    bundle.text_section_for_addr(&artifact, block.start_addr)
                else {
                    return;
                };
                let display = format!("0x{:x}", block.start_addr);
                shell.open_link_context_menu(
                    artifact.clone(),
                    section.to_string(),
                    block.start_addr,
                    false,
                    display,
                    pos,
                    cx,
                );
            },
        )
    });

    let hooks = CameraHooks {
        pan_by: Box::new(|shell, dx, dy, cx| shell.cfg_pan_by(dx, dy, cx)),
        zoom_by: Box::new(|shell, anchor, delta, cx| {
            shell.cfg_zoom_by(anchor, delta, cx)
        }),
        drag_start: Box::new(|shell, pos| shell.cfg_drag_start(pos)),
        drag_move: Box::new(|shell, pos, cx| shell.cfg_drag_move(pos, cx)),
        drag_end: Box::new(|shell| shell.cfg_drag_end()),
        set_bounds: Box::new(|shell, bounds| {
            if let Some(idx) = shell.active_tab {
                if let Some(tab) = shell.tabs.get_mut(idx) {
                    if let Some(view) = tab.cfg.as_mut() {
                        view.camera.viewport_bounds = bounds;
                    }
                }
            }
        }),
    };

    let header_label = SharedString::from(func_name);
    let header_subtitle = SharedString::from(format!(
        "{} blocks · {} edges · zoom {:.0}%",
        cfg.blocks.len(),
        cfg.edges.len(),
        zoom * 100.,
    ));

    render_graph_canvas(
        &scene,
        &view.camera,
        panel,
        border,
        fg,
        dim,
        "cfg",
        header_label,
        Some(header_subtitle),
        content,
        node_click,
        node_right_click,
        None, // node_hover
        hooks,
        cx,
    )
}

// ---- Block layout planning -------------------------------------------------

// Physical text metrics — rounded up from gpui's rendered sizes plus
// a couple of px of slack on each row so subpixel rounding can't clip
// the last instruction.
const ROW_PX: f32 = 17.;
const ELLIPSIS_ROW_PX: f32 = 28.;
const PADDING_PX_H: f32 = 18.;
const HEIGHT_FUDGE_PX: f32 = 4.;
const CHAR_PX: f32 = 7.;
const PADDING_PX_W: f32 = 28.;
const MIN_BLOCK_PX_W: f32 = 80.;
const MAX_BLOCK_PX_W: f32 = 640.;
/// World-space height budget for a full (truncated) block. At zoom = 1
/// this is `FULL_BLOCK_WORLD_H * WORLD_UNIT` pixels.
const FULL_BLOCK_WORLD_H: f32 = 0.6;

fn plan_blocks(
    cfg: &glass_arch_arm64::FunctionCfg,
    summaries: &[CfgBlockSummary],
    unit: f32,
) -> Vec<CfgLayoutPlan> {
    let full_block_px_h = FULL_BLOCK_WORLD_H * unit;
    let budget_px_h = full_block_px_h - HEIGHT_FUDGE_PX;
    cfg.blocks
        .iter()
        .zip(summaries.iter())
        .map(|(b, s)| plan_one(b, s.symbol.is_some(), budget_px_h))
        .collect()
}

fn plan_one(
    b: &glass_arch_arm64::BasicBlock,
    has_symbol: bool,
    budget_px_h: f32,
) -> CfgLayoutPlan {
    let n = b.instructions.len();
    if n == 0 {
        return CfgLayoutPlan {
            preview: 0,
            show_ellipsis: false,
            show_last: false,
        };
    }
    let sym_h = if has_symbol { ROW_PX } else { 0. };
    let full_h = sym_h + (n as f32) * ROW_PX + PADDING_PX_H;
    if full_h <= budget_px_h {
        return CfgLayoutPlan {
            preview: n,
            show_ellipsis: false,
            show_last: false,
        };
    }
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
    if sym_h + ELLIPSIS_ROW_PX + ROW_PX + PADDING_PX_H <= budget_px_h {
        return CfgLayoutPlan {
            preview: 0,
            show_ellipsis: true,
            show_last: true,
        };
    }
    if sym_h + ROW_PX + PADDING_PX_H <= budget_px_h {
        return CfgLayoutPlan {
            preview: 0,
            show_ellipsis: false,
            show_last: true,
        };
    }
    CfgLayoutPlan {
        preview: 0,
        show_ellipsis: true,
        show_last: false,
    }
}

fn size_block_px(
    block: &glass_arch_arm64::BasicBlock,
    summary: &CfgBlockSummary,
    plan: CfgLayoutPlan,
    _unit: f32,
) -> (f32, f32) {
    const ADDR_COL: usize = 16 + 1; // "0123456789abcdef "
    let mut longest = 0usize;
    if let Some(name) = summary.symbol.as_ref() {
        longest = longest.max(name.len() + 1);
    }
    let insn_line_len = |insn: &glass_arch_arm64::InstructionEntry| -> usize {
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
    let w_px =
        ((longest as f32) * CHAR_PX + PADDING_PX_W).clamp(MIN_BLOCK_PX_W, MAX_BLOCK_PX_W);

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
        content_px += ROW_PX;
    }
    let h_px = content_px.max(ROW_PX + PADDING_PX_H);
    (w_px, h_px)
}
