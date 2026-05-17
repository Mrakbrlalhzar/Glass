//! Per-frame renderer for the DEX call-graph view.
//!
//! Thin shell over `graph_canvas::render_graph_canvas`. The DEX-
//! specific bits are:
//!   * Per-node content (class header + method name + signature +
//!     instruction-count footer).
//!   * Hover over a node expands its un-placed callees in place
//!     (one-way; entering a new node doesn't collapse anything).
//!   * Left-click on a node jumps to the method's definition in the
//!     class smali tab.
//!   * Camera + drag wired to the `dex_callgraph` tab state.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{div, prelude::*, rgb, AnyElement, Context, SharedString};

use crate::dex_callgraph::DexNodeInfo;
use crate::graph::NodeId;
use crate::graph_canvas::{
    render_graph_canvas, CameraHooks, NodeClickFn, NodeContentFn, NodeHoverFn,
    NodeRightClickFn,
};
use crate::palette::{COLOUR_BYTES, COLOUR_PLAIN, COLOUR_SYMBOL_HEADER};
use crate::{LeafId, LoadedBundle, Shell};

pub fn render_dex_callgraph(
    shell: &mut Shell,
    bundle: &LoadedBundle,
    class_jni: &str,
    method_decl: &str,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    _accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    shell.ensure_dex_callgraph_built(class_jni, method_decl);
    let method_lines: Arc<HashMap<String, (LeafId, usize)>> =
        Arc::new((*bundle.method_lines).clone());

    let Some(active_idx) = shell.active_tab else {
        return div().size_full().bg(panel).into_any_element();
    };
    let view = shell
        .tabs
        .get(active_idx)
        .and_then(|t| t.dex_callgraph.as_ref())
        .cloned();
    let Some(view) = view else {
        return div().size_full().bg(panel).into_any_element();
    };

    let zoom = view.zoom();
    let node_count = view.scene.nodes.len();
    let keys: Arc<Vec<String>> = Arc::new(view.keys.clone());
    let info: Arc<Vec<DexNodeInfo>> = Arc::new(view.info.clone());

    let content: NodeContentFn = {
        let info = info.clone();
        Box::new(move |nid: NodeId, _rect, _weak| {
            let Some(node_info) = info.get(nid.0) else {
                return div().into_any_element();
            };
            let mut body = div()
                .size_full()
                .bg(gpui::rgba(0x2a313cff))
                .border_2()
                .border_color(rgb(0x6b6b78))
                .rounded_sm()
                .px_2()
                .py_1()
                .flex()
                .flex_col()
                .overflow_hidden()
                .text_xs()
                .font_family("Courier New");

            // Class header (yellow, matches CFG symbol header).
            body = body.child(
                div()
                    .text_color(rgb(COLOUR_SYMBOL_HEADER))
                    .whitespace_nowrap()
                    .child(SharedString::from(format!(
                        "{}:",
                        node_info.class_name
                    ))),
            );

            // Method + signature on one line.
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .text_color(rgb(COLOUR_PLAIN))
                            .child(node_info.method_name.clone()),
                    )
                    .child(
                        div()
                            .text_color(rgb(COLOUR_BYTES))
                            .child(node_info.signature.clone()),
                    ),
            );

            // Footer: instruction count or "external".
            let footer = match node_info.instruction_count {
                Some(n) => SharedString::from(format!("{n} insns")),
                None => SharedString::from("external"),
            };
            body = body.child(
                div()
                    .mt_auto()
                    .text_color(rgb(COLOUR_BYTES))
                    .whitespace_nowrap()
                    .child(footer),
            );

            body.into_any_element()
        })
    };

    // Left-click jumps to the method's definition in its smali tab.
    // Methods we don't have source for (framework / external) have no
    // entry in `method_lines` and the click is a no-op. Shift-click
    // is honoured for consistency but smali tabs dedupe by class so
    // it ends up as a regular navigation.
    let node_click: Option<NodeClickFn> = Some({
        let keys = keys.clone();
        let method_lines = method_lines.clone();
        Box::new(
            move |shell: &mut Shell,
                  nid: NodeId,
                  _mods: gpui::Modifiers,
                  cx: &mut Context<Shell>| {
                let Some(key) = keys.get(nid.0) else { return };
                let Some(&(leaf, line)) = method_lines.get(key) else { return };
                shell.goto_smali_method(leaf, line, cx);
            },
        )
    });

    // Right-click on a node → context menu with Follow / Follow in
    // new tab, matching the listing's link menu. Methods we don't
    // have source for skip the menu.
    let node_right_click: Option<NodeRightClickFn> = Some({
        let keys = keys.clone();
        let method_lines = method_lines.clone();
        Box::new(
            move |shell: &mut Shell,
                  nid: NodeId,
                  pos: gpui::Point<gpui::Pixels>,
                  cx: &mut Context<Shell>| {
                let Some(key) = keys.get(nid.0).cloned() else { return };
                let Some(&(leaf, line)) = method_lines.get(&key) else { return };
                // Human-readable label: ClassName.method.
                let label = key
                    .split("->")
                    .nth(1)
                    .and_then(|m| m.split('(').next())
                    .unwrap_or(&key)
                    .to_string();
                shell.open_smali_link_context_menu(leaf, line, label, pos, cx);
            },
        )
    });

    // Hover expands. One-shot: re-entering a node that's already been
    // expanded is a no-op inside `expand_dex_callee` (it filters to
    // un-placed callees). Leaving the node does nothing.
    let node_hover: Option<NodeHoverFn> = Some({
        let keys = keys.clone();
        Box::new(move |shell: &mut Shell, nid: NodeId, cx: &mut Context<Shell>| {
            if let Some(key) = keys.get(nid.0).cloned() {
                shell.expand_dex_callee(&key, cx);
            }
        })
    });

    let hooks = CameraHooks {
        pan_by: Box::new(|shell, dx, dy, cx| shell.dex_cg_pan_by(dx, dy, cx)),
        zoom_by: Box::new(|shell, anchor, delta, cx| {
            shell.dex_cg_zoom_by(anchor, delta, cx)
        }),
        drag_start: Box::new(|shell, pos| shell.dex_cg_drag_start(pos)),
        drag_move: Box::new(|shell, pos, cx| shell.dex_cg_drag_move(pos, cx)),
        drag_end: Box::new(|shell| shell.dex_cg_drag_end()),
        set_bounds: Box::new(|shell, bounds| {
            if let Some(idx) = shell.active_tab {
                if let Some(tab) = shell.tabs.get_mut(idx) {
                    if let Some(view) = tab.dex_callgraph.as_mut() {
                        view.camera.viewport_bounds = bounds;
                    }
                }
            }
        }),
    };

    let header_label = SharedString::from(format!("{class_jni}->{method_decl}"));
    let header_subtitle = SharedString::from(format!(
        "{} methods · zoom {:.0}%",
        node_count,
        zoom * 100.,
    ));

    render_graph_canvas(
        &view.scene,
        &view.camera,
        panel,
        border,
        fg,
        dim,
        "dex-cg",
        header_label,
        Some(header_subtitle),
        content,
        node_click,
        node_right_click,
        node_hover,
        hooks,
        cx,
    )
}
