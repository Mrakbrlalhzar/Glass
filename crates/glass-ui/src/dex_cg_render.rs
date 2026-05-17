//! Per-frame renderer for the DEX call-graph view.

use gpui::{div, prelude::*, px, rgb, App, Context, Pixels, SharedString};

use crate::graph;
use crate::palette::{COLOUR_BYTES, COLOUR_SYMBOL_HEADER};
use crate::{LoadedBundle, Shell};

pub fn render_dex_callgraph(
    shell: &mut Shell,
    bundle: &LoadedBundle,
    class_jni: &str,
    method_decl: &str,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    let _ = (border, dim, accent);
    let _ = bundle;
        shell.ensure_dex_callgraph_built(class_jni, method_decl);

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

        let weak = cx.entity().downgrade();
        let zoom = view.zoom();

        // Use the shared graph machinery for camera math, layout
        // rects and edge routing.
        let rects = graph::compute_node_rects(&view.scene, &view.camera);
        let routed = graph::route_edges(&view.scene, &rects);
        let bounds_unknown = graph::bounds_unknown(&view.camera);
        let bounds_w = view.camera.viewport_bounds.size.width.as_f32();
        let bounds_h = view.camera.viewport_bounds.size.height.as_f32();

        let mut scene_div = div()
            .id("dex-callgraph-scene")
            .absolute()
            .top_0()
            .left_0()
            .size_full();

        // Edges (rendered first so nodes paint over them).
        for (edge, route) in view.scene.edges.iter().zip(routed.iter()) {
            for seg in &route.segments {
                scene_div =
                    scene_div.child(graph::render_edge_segment(*seg, edge.style, edge.kind));
            }
            scene_div = scene_div.child(graph::render_edge_arrowhead(
                route.arrow_tip.0,
                route.arrow_tip.1,
                route.arrow_dir,
                edge.kind,
            ));
        }

        // Nodes — content rendered inline (DEX-specific styling).
        for (i, node) in view.scene.nodes.iter().enumerate() {
            let rect = rects[i];
            let cull = !bounds_unknown
                && (rect.x + rect.w < 0.
                    || rect.x > bounds_w
                    || rect.y + rect.h < 0.
                    || rect.y > bounds_h);
            if cull {
                continue;
            }
            let key = view.keys.get(i).cloned().unwrap_or_default();
            let display_name = key
                .split("->")
                .nth(1)
                .and_then(|m| m.split('(').next())
                .map(|s| s.to_string())
                .unwrap_or_else(|| node.label.to_string());
            let class_name = key
                .split(';')
                .next()
                .and_then(|s| s.split('/').next_back())
                .unwrap_or("")
                .to_string();
            let click_weak = weak.clone();
            let click_key = key.clone();
            scene_div = scene_div.child(
                div()
                    .id(("dex-cg-node", i))
                    .absolute()
                    .left(px(rect.x))
                    .top(px(rect.y))
                    .w(px(rect.w))
                    .h(px(rect.h))
                    .bg(gpui::rgba(0x2a313cff))
                    .border_2()
                    .border_color(rgb(0x6b6b78))
                    .rounded_sm()
                    .px_2()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .overflow_hidden()
                    .cursor_pointer()
                    .text_xs()
                    .font_family("Courier New")
                    .child(
                        div()
                            .text_color(rgb(COLOUR_SYMBOL_HEADER))
                            .whitespace_nowrap()
                            .child(SharedString::from(display_name)),
                    )
                    .child(
                        div()
                            .text_color(rgb(COLOUR_BYTES))
                            .whitespace_nowrap()
                            .child(SharedString::from(class_name)),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_ev, _w, cx: &mut App| {
                            if let Some(entity) = click_weak.upgrade() {
                                let key = click_key.clone();
                                cx.update_entity(&entity, |shell, cx| {
                                    shell.expand_dex_callee(&key, cx);
                                });
                            }
                        },
                    ),
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
                                if let Some(view) = tab.dex_callgraph.as_mut() {
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

        let header_label = format!(
            "{class_jni}->{method_decl}  ·  {} methods  ·  zoom {:.0}%",
            view.scene.nodes.len(),
            zoom * 100.,
        );
        let header = div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .px_3()
            .border_b_1()
            .border_color(rgb(0x36363c))
            .text_sm()
            .text_color(fg)
            .font_family("Menlo")
            .child(SharedString::from(header_label));

        let zoom_weak = weak.clone();
        let drag_weak = weak.clone();
        let drag_move_weak = weak.clone();
        let drag_end_weak = weak.clone();

        let canvas_body = div()
            .id("dex-cg-canvas")
            .flex_1()
            .relative()
            .overflow_hidden()
            .bg(panel)
            .child(measure)
            .child(scene_div)
            .on_scroll_wheel(move |ev: &gpui::ScrollWheelEvent, _w, cx| {
                if let Some(entity) = zoom_weak.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        let delta = ev.delta.pixel_delta(px(20.));
                        if ev.modifiers.shift
                            || ev.modifiers.platform
                            || ev.modifiers.control
                        {
                            let raw = if delta.y.as_f32().abs() > 0. {
                                delta.y.as_f32()
                            } else {
                                delta.x.as_f32()
                            };
                            shell.dex_cg_zoom_by(ev.position, raw, cx);
                        } else {
                            shell.dex_cg_pan_by(delta.x.as_f32(), delta.y.as_f32(), cx);
                        }
                    });
                }
            })
            .on_mouse_down(
                gpui::MouseButton::Middle,
                move |ev: &gpui::MouseDownEvent, _w, cx| {
                    if let Some(entity) = drag_weak.upgrade() {
                        let pos = ev.position;
                        cx.update_entity(&entity, |shell, _cx| {
                            shell.dex_cg_drag_start(pos);
                        });
                    }
                },
            )
            .on_mouse_move(move |ev: &gpui::MouseMoveEvent, _w, cx| {
                if let Some(entity) = drag_move_weak.upgrade() {
                    let pos = ev.position;
                    cx.update_entity(&entity, |shell, cx| {
                        shell.dex_cg_drag_move(pos, cx);
                    });
                }
            })
            .on_mouse_up(
                gpui::MouseButton::Middle,
                move |_ev, _w, cx| {
                    if let Some(entity) = drag_end_weak.upgrade() {
                        cx.update_entity(&entity, |shell, _cx| {
                            shell.dex_cg_drag_end();
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
