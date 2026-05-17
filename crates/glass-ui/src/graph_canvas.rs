//! Shared canvas for graph views — header + measure-canvas + scene
//! body + scroll/zoom/pan/drag event handlers.
//!
//! Both the CFG view and the DEX call-graph view render through this.
//! Each one supplies:
//!
//!   * A per-frame `GraphScene` snapshot (caller owns mutation).
//!   * A clone of the `GraphCamera` so the canvas can read pan / zoom /
//!     viewport bounds.
//!   * A header string for the top strip.
//!   * Per-node content render callback (turns a `NodeId` + screen
//!     rect into the inner element).
//!   * Per-node click callback (left-click). Optional.
//!   * A set of Shell callbacks that wire mouse events back into the
//!     view's camera + tab state via the gpui entity.
//!
//! The result is a single ready-to-paint `AnyElement`. The view-
//! specific module just plugs in its callbacks and gets the shared
//! look + behaviour for free.

use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Pixels, SharedString, WeakEntity,
};

use crate::graph::{
    bounds_unknown, compute_node_rects, render_edge_arrowhead, render_edge_segment,
    route_edges, GraphCamera, GraphScene, NodeId, NodeRect,
};
use crate::Shell;

/// Per-canvas wiring for forwarding camera events back into Shell.
///
/// Each closure takes the gpui `App` so it can resolve the weak entity
/// and call into the tab's state. Callers pass closures that mutate
/// the right `Tab`'s camera (CFG vs DEX call graph).
#[allow(clippy::type_complexity)]
pub struct CameraHooks {
    pub pan_by: Box<dyn Fn(&mut Shell, f32, f32, &mut Context<Shell>)>,
    pub zoom_by:
        Box<dyn Fn(&mut Shell, gpui::Point<Pixels>, f32, &mut Context<Shell>)>,
    pub drag_start: Box<dyn Fn(&mut Shell, gpui::Point<Pixels>)>,
    pub drag_move: Box<dyn Fn(&mut Shell, gpui::Point<Pixels>, &mut Context<Shell>)>,
    pub drag_end: Box<dyn Fn(&mut Shell)>,
    /// Receives the canvas's window-coordinate bounds on every paint.
    /// The hook should write them into the tab's camera so pan/zoom
    /// math has fresh values next frame.
    pub set_bounds:
        Box<dyn Fn(&mut Shell, gpui::Bounds<Pixels>)>,
}

/// Optional node-click callback (left mouse button). Receives the
/// Shell entity context and the `NodeId` the user clicked on.
pub type NodeClickFn =
    Box<dyn Fn(&mut Shell, NodeId, &mut Context<Shell>)>;

/// Optional node-hover callback. Fires once when the cursor enters
/// the node's rect. The caller decides whether to do anything on
/// exit (we don't pass the `hovered` flag through — that's an
/// intentional simplification for the "hover to expand, no collapse"
/// use case the DEX call graph wants).
pub type NodeHoverFn =
    Box<dyn Fn(&mut Shell, NodeId, &mut Context<Shell>)>;

/// Per-node content render — turns a placed rect + node id into a
/// fully-styled element. The element should fill the rect (the
/// canvas just positions it absolutely).
pub type NodeContentFn = Box<dyn Fn(NodeId, NodeRect, WeakEntity<Shell>) -> AnyElement>;

/// Render the canvas. Owns the header, the measure-canvas (writes
/// fresh viewport bounds each frame), the absolute-positioned scene
/// body, and the wheel/drag mouse handlers.
pub fn render_graph_canvas(
    scene: &GraphScene,
    camera: &GraphCamera,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    id_prefix: &'static str,
    header_label: SharedString,
    header_subtitle: Option<SharedString>,
    content: NodeContentFn,
    node_click: Option<NodeClickFn>,
    node_hover: Option<NodeHoverFn>,
    hooks: CameraHooks,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let weak = cx.entity().downgrade();

    let rects = compute_node_rects(scene, camera);
    let routed = route_edges(scene, &rects);
    let bounds_unk = bounds_unknown(camera);
    let bounds_w = camera.viewport_bounds.size.width.as_f32();
    let bounds_h = camera.viewport_bounds.size.height.as_f32();

    let mut scene_div = div()
        .id(id_prefix)
        .absolute()
        .top_0()
        .left_0()
        .size_full();

    // Edges first so nodes paint over them.
    for (edge, route) in scene.edges.iter().zip(routed.iter()) {
        for seg in &route.segments {
            scene_div =
                scene_div.child(render_edge_segment(*seg, edge.style, edge.kind));
        }
        scene_div = scene_div.child(render_edge_arrowhead(
            route.arrow_tip.0,
            route.arrow_tip.1,
            route.arrow_dir,
            edge.kind,
        ));
    }

    // Nodes — culled if entirely off-canvas (skipped on first paint
    // when bounds are unknown).
    let click_arc: Option<std::sync::Arc<NodeClickFn>> = node_click.map(std::sync::Arc::new);
    let hover_arc: Option<std::sync::Arc<NodeHoverFn>> = node_hover.map(std::sync::Arc::new);
    for (i, _node) in scene.nodes.iter().enumerate() {
        let rect = rects[i];
        let cull = !bounds_unk
            && (rect.x + rect.w < 0.
                || rect.x > bounds_w
                || rect.y + rect.h < 0.
                || rect.y > bounds_h);
        if cull {
            continue;
        }
        let body = content(NodeId(i), rect, weak.clone());
        let mut wrapper = div()
            .id((id_prefix, i))
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.w))
            .h(px(rect.h))
            .cursor_pointer()
            .child(body);
        if let Some(click) = click_arc.clone() {
            let click_weak = weak.clone();
            let nid = NodeId(i);
            wrapper = wrapper.on_mouse_down(
                gpui::MouseButton::Left,
                move |_ev, _w, cx: &mut App| {
                    if let Some(entity) = click_weak.upgrade() {
                        let click = click.clone();
                        cx.update_entity(&entity, |shell, cx| {
                            click(shell, nid, cx);
                        });
                    }
                },
            );
        }
        if let Some(hover) = hover_arc.clone() {
            let hover_weak = weak.clone();
            let nid = NodeId(i);
            wrapper = wrapper.on_hover(move |&hovered: &bool, _w, cx: &mut App| {
                if !hovered {
                    return;
                }
                if let Some(entity) = hover_weak.upgrade() {
                    let hover = hover.clone();
                    cx.update_entity(&entity, |shell, cx| {
                        hover(shell, nid, cx);
                    });
                }
            });
        }
        scene_div = scene_div.child(wrapper);
    }

    // Measure canvas: writes fresh viewport bounds back into the
    // tab's camera on every paint.
    let bounds_weak = weak.clone();
    let set_bounds = std::sync::Arc::new(hooks.set_bounds);
    let measure = gpui::canvas(
        move |bounds, _window, cx| {
            if let Some(entity) = bounds_weak.upgrade() {
                let set_bounds = set_bounds.clone();
                cx.update_entity(&entity, |shell, _cx| {
                    set_bounds(shell, bounds);
                });
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    let header_inner = div()
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
        .child(header_label);
    let header = if let Some(sub) = header_subtitle {
        header_inner.child(div().text_color(dim).child(sub))
    } else {
        header_inner
    };

    let pan_by = std::sync::Arc::new(hooks.pan_by);
    let zoom_by = std::sync::Arc::new(hooks.zoom_by);
    let drag_start = std::sync::Arc::new(hooks.drag_start);
    let drag_move = std::sync::Arc::new(hooks.drag_move);
    let drag_end = std::sync::Arc::new(hooks.drag_end);

    let wheel_weak = weak.clone();
    let down_weak = weak.clone();
    let move_weak = weak.clone();
    let up_weak = weak.clone();

    let wheel_pan = pan_by.clone();
    let wheel_zoom = zoom_by.clone();
    let down_start = drag_start.clone();
    let move_drag = drag_move.clone();
    let up_end = drag_end.clone();

    let canvas_body = div()
        .id(id_prefix)
        .flex_1()
        .relative()
        .overflow_hidden()
        .bg(panel)
        .child(measure)
        .child(scene_div)
        .on_scroll_wheel(move |ev: &gpui::ScrollWheelEvent, _w, cx| {
            if let Some(entity) = wheel_weak.upgrade() {
                let pan = wheel_pan.clone();
                let zoom = wheel_zoom.clone();
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
                        zoom(shell, ev.position, raw, cx);
                    } else {
                        pan(shell, delta.x.as_f32(), delta.y.as_f32(), cx);
                    }
                });
            }
        })
        .on_mouse_down(
            gpui::MouseButton::Middle,
            move |ev: &gpui::MouseDownEvent, _w, cx| {
                if let Some(entity) = down_weak.upgrade() {
                    let pos = ev.position;
                    let start = down_start.clone();
                    cx.update_entity(&entity, |shell, _cx| {
                        start(shell, pos);
                    });
                }
            },
        )
        .on_mouse_move(move |ev: &gpui::MouseMoveEvent, _w, cx| {
            if let Some(entity) = move_weak.upgrade() {
                let pos = ev.position;
                let drag = move_drag.clone();
                cx.update_entity(&entity, |shell, cx| {
                    drag(shell, pos, cx);
                });
            }
        })
        .on_mouse_up(
            gpui::MouseButton::Middle,
            move |_ev, _w, cx| {
                if let Some(entity) = up_weak.upgrade() {
                    let end = up_end.clone();
                    cx.update_entity(&entity, |shell, _cx| {
                        end(shell);
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
