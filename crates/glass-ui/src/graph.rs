//! Shared scene + canvas for graph views.
//!
//! Both the native CFG and the DEX call graph render "tagged
//! rectangular nodes connected by edges on a pannable / zoomable
//! canvas." Everything that's view-agnostic lives here:
//!
//!   * [`GraphCamera`] — pan + zoom + drag bookkeeping, with the
//!     world↔screen conversion helpers.
//!   * [`GraphScene`] / [`GraphNode`] / [`GraphEdge`] — the
//!     intermediate representation. Callers fill these in with
//!     whatever node and edge semantics they need.
//!   * Edge router (3- or 5-segment Manhattan with rank-gap lanes
//!     and side highways for back-edges or multi-rank edges) lives
//!     here too — both consumers want it.
//!   * Arrowhead and segment primitives.
//!
//! Caller-specific logic (per-node content rendering, click
//! handlers, click-to-expand) plugs in via callbacks. The Tab /
//! TabState plumbing stays in the parent module because each
//! tab kind persists slightly different state.

use std::collections::{BTreeMap, HashMap};

use gpui::{
    div, prelude::*, px, rgb, Bounds, Pixels, Point, SharedString,
};

// ---- Camera & world coordinates --------------------------------------------

/// Pixels per world unit at zoom = 1.0. World coords are normalised
/// against this; a node at world `(x, y)` maps to screen pixel
/// `(viewport_centre + (x - pan) * WORLD_UNIT * zoom)`.
pub const WORLD_UNIT: f32 = 180.;
pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 4.;
/// Per-notch zoom multiplier for a single scroll-wheel / pinch step.
pub const ZOOM_STEP: f32 = 1.1;

/// Camera state shared by all graph views. Persisted alongside each
/// tab's specific state so reopening a tab restores the viewport.
#[derive(Clone, Debug)]
pub struct GraphCamera {
    pub pan_x: f32,
    pub pan_y: f32,
    pub zoom: f32,
    /// Viewport bounds in *window* coordinates, captured by a
    /// `canvas` hook each frame so pan/zoom math has fresh values.
    /// Defaulted at construction; the first paint overwrites it.
    pub viewport_bounds: Bounds<Pixels>,
    /// `Some(start_pos, start_pan_x, start_pan_y)` while the user is
    /// mid pan-drag. `None` otherwise.
    pub drag_start: Option<(Point<Pixels>, f32, f32)>,
}

impl GraphCamera {
    pub fn new(pan_x: f32, pan_y: f32, zoom: f32) -> Self {
        Self {
            pan_x,
            pan_y,
            zoom: zoom.clamp(MIN_ZOOM, MAX_ZOOM),
            viewport_bounds: Bounds::default(),
            drag_start: None,
        }
    }

    /// Screen pixels per world unit at the current zoom.
    pub fn unit(&self) -> f32 {
        WORLD_UNIT * self.zoom
    }

    pub fn pan_by(&mut self, dx_px: f32, dy_px: f32) {
        let unit = self.unit();
        if unit <= 0. {
            return;
        }
        self.pan_x -= dx_px / unit;
        self.pan_y -= dy_px / unit;
    }

    /// Zoom anchored at a window-coordinate `anchor`. Positive
    /// `delta` zooms in, negative out.
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
        let old_unit = WORLD_UNIT * old_zoom;
        let new_unit = WORLD_UNIT * new_zoom;
        let ax = anchor.x.as_f32();
        let ay = anchor.y.as_f32();
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
        self.pan_x = start_pan_x - (pos.x.as_f32() - start_pos.x.as_f32()) / unit;
        self.pan_y = start_pan_y - (pos.y.as_f32() - start_pos.y.as_f32()) / unit;
    }

    pub fn drag_end(&mut self) {
        self.drag_start = None;
    }
}

// ---- Scene model -----------------------------------------------------------

/// Opaque identifier for a node within a scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

#[derive(Clone, Debug)]
pub struct GraphNode {
    pub id: NodeId,
    /// Display label — used for the tab title and debug logs; the
    /// actual on-canvas content is whatever the caller's content
    /// callback produces.
    pub label: SharedString,
    /// Layout hints (size, rank, etc.) used by the placement pass.
    pub hints: NodeHints,
    pub tags: NodeTags,
}

#[derive(Clone, Debug)]
pub struct NodeHints {
    /// Caller's preferred on-screen size for this node, in pixels.
    /// The layout enforces spacing between nodes from this. Real
    /// rendering size comes from the content callback — they should
    /// agree.
    pub size_px: (f32, f32),
    /// Explicit rank to pin the node to (0 = root). When `None`, the
    /// layout assigns ranks via BFS distance from the root.
    pub rank: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct NodeTags {
    /// Warm tint — exit / terminal blocks in a CFG, leaf methods
    /// in a call graph.
    pub is_exit: bool,
    /// Brighter border — entry block in a CFG, root in a call graph.
    pub is_entry: bool,
}

#[derive(Clone, Debug)]
pub struct GraphEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub style: EdgeStyle,
    pub kind: EdgeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeStyle {
    Solid,
    Dotted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// Intra-function control flow.
    ControlFlow,
    /// Cross-function call.
    Call,
}

#[derive(Clone, Debug, Default)]
pub struct GraphScene {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Caller-controlled per-node screen-space position (top-left
    /// of the node, in world units relative to scene origin).
    /// `None` entries are placed automatically by [`layout_scene`].
    pub positions: Vec<Option<(f32, f32)>>,
}

impl GraphScene {
    /// Add a node and return its assigned id. The caller can store
    /// the id and use it in subsequent edges.
    pub fn add_node(&mut self, label: impl Into<SharedString>, hints: NodeHints, tags: NodeTags) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(GraphNode { id, label: label.into(), hints, tags });
        self.positions.push(None);
        id
    }

    pub fn add_edge(&mut self, from: NodeId, to: NodeId, style: EdgeStyle, kind: EdgeKind) {
        self.edges.push(GraphEdge { from, to, style, kind });
    }
}

// ---- Edge rendering primitives ---------------------------------------------

/// One horizontal or vertical line segment in screen-local pixel
/// coordinates. Edge routers produce a sequence of these per edge.
#[derive(Clone, Copy, Debug)]
pub struct EdgeSegment {
    pub x: f32,
    pub y: f32,
    pub length: f32,
    pub horizontal: bool,
}

pub const EDGE_COLOR_CONTROL: u32 = 0x9aa3b3;
pub const EDGE_COLOR_CONTROL_DOTTED: u32 = 0x6e7382;
pub const EDGE_COLOR_CALL: u32 = 0x66c2ff;
pub const EDGE_THICKNESS: f32 = 2.;
const DOT_LEN: f32 = 4.;
const DOT_GAP: f32 = 3.;

/// Colour an edge gets at its current style + kind.
pub fn edge_colour(style: EdgeStyle, kind: EdgeKind) -> gpui::Rgba {
    let base = match kind {
        EdgeKind::Call => EDGE_COLOR_CALL,
        EdgeKind::ControlFlow => match style {
            EdgeStyle::Solid => EDGE_COLOR_CONTROL,
            EdgeStyle::Dotted => EDGE_COLOR_CONTROL_DOTTED,
        },
    };
    gpui::rgba((base << 8) | 0xee)
}

pub fn render_edge_segment(seg: EdgeSegment, style: EdgeStyle, kind: EdgeKind) -> gpui::Div {
    let colour = edge_colour(style, kind);
    if style == EdgeStyle::Dotted {
        // Dotted = stacked short rectangles. Container's an
        // absolute box; bars are positioned inside it.
        let mut wrapper = div()
            .absolute()
            .left(px(seg.x - EDGE_THICKNESS / 2.))
            .top(px(seg.y - EDGE_THICKNESS / 2.));
        let stride = DOT_LEN + DOT_GAP;
        let mut pos = 0.0_f32;
        let length = seg.length;
        if seg.horizontal {
            wrapper = wrapper.w(px(length + EDGE_THICKNESS)).h(px(EDGE_THICKNESS));
            while pos < length {
                let len = DOT_LEN.min(length - pos);
                wrapper = wrapper.child(
                    div()
                        .absolute()
                        .left(px(pos))
                        .top(px(0.))
                        .w(px(len))
                        .h(px(EDGE_THICKNESS))
                        .bg(colour),
                );
                pos += stride;
            }
        } else {
            wrapper = wrapper.w(px(EDGE_THICKNESS)).h(px(length + EDGE_THICKNESS));
            while pos < length {
                let len = DOT_LEN.min(length - pos);
                wrapper = wrapper.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px(pos))
                        .w(px(EDGE_THICKNESS))
                        .h(px(len))
                        .bg(colour),
                );
                pos += stride;
            }
        }
        wrapper
    } else if seg.horizontal {
        div()
            .absolute()
            .left(px(seg.x))
            .top(px(seg.y - EDGE_THICKNESS / 2.))
            .w(px(seg.length))
            .h(px(EDGE_THICKNESS))
            .bg(colour)
    } else {
        div()
            .absolute()
            .left(px(seg.x - EDGE_THICKNESS / 2.))
            .top(px(seg.y))
            .w(px(EDGE_THICKNESS))
            .h(px(seg.length))
            .bg(colour)
    }
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub enum ArrowHeadDir {
    Down,
    Up,
    Left,
    Right,
}

pub fn render_edge_arrowhead(
    tip_x: f32,
    tip_y: f32,
    dir: ArrowHeadDir,
    kind: EdgeKind,
) -> gpui::Div {
    const HEAD_HALF: f32 = 5.;
    const HEAD_LEN: f32 = 7.;
    let colour = edge_colour(EdgeStyle::Solid, kind);
    let (left, top, w, h) = match dir {
        ArrowHeadDir::Down => {
            (tip_x - HEAD_HALF, tip_y - HEAD_LEN, HEAD_HALF * 2., HEAD_LEN)
        }
        ArrowHeadDir::Up => (tip_x - HEAD_HALF, tip_y, HEAD_HALF * 2., HEAD_LEN),
        ArrowHeadDir::Left => (tip_x, tip_y - HEAD_HALF, HEAD_LEN, HEAD_HALF * 2.),
        ArrowHeadDir::Right => {
            (tip_x - HEAD_LEN, tip_y - HEAD_HALF, HEAD_LEN, HEAD_HALF * 2.)
        }
    };
    let mut head = div().absolute().left(px(left)).top(px(top)).w(px(w)).h(px(h));
    let half = HEAD_HALF as i32;
    for k in -half..=half {
        let abs_k = k.unsigned_abs() as f32;
        let bar_len = HEAD_LEN * (1.0 - abs_k / (half as f32));
        if bar_len <= 0. {
            continue;
        }
        match dir {
            ArrowHeadDir::Down => {
                let bar_left = (k as f32) + HEAD_HALF - 0.5;
                head = head.child(
                    div()
                        .absolute()
                        .left(px(bar_left))
                        .top(px(0.))
                        .w(px(1.))
                        .h(px(bar_len))
                        .bg(colour),
                );
            }
            ArrowHeadDir::Up => {
                let bar_left = (k as f32) + HEAD_HALF - 0.5;
                head = head.child(
                    div()
                        .absolute()
                        .left(px(bar_left))
                        .top(px(HEAD_LEN - bar_len))
                        .w(px(1.))
                        .h(px(bar_len))
                        .bg(colour),
                );
            }
            ArrowHeadDir::Right => {
                let bar_top = (k as f32) + HEAD_HALF - 0.5;
                head = head.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px(bar_top))
                        .w(px(bar_len))
                        .h(px(1.))
                        .bg(colour),
                );
            }
            ArrowHeadDir::Left => {
                let bar_top = (k as f32) + HEAD_HALF - 0.5;
                head = head.child(
                    div()
                        .absolute()
                        .left(px(HEAD_LEN - bar_len))
                        .top(px(bar_top))
                        .w(px(bar_len))
                        .h(px(1.))
                        .bg(colour),
                );
            }
        }
    }
    head
}

// ---- Placement -------------------------------------------------------------

/// Lay out every node whose `positions[i] == None`. Nodes already
/// positioned by the caller are left alone (used for "place this
/// callee next to the caller I clicked on" workflows).
///
/// The algorithm:
/// 1. Rank assignment by BFS from the root (node 0) over forward edges.
/// 2. Barycenter sweeps (top-down + bottom-up) for crossing minimisation.
/// 3. Jacobi relaxation to align parents and children.
/// 4. Per-rank min-gap enforcement using each node's pixel size.
///
/// All positions returned are in world units. The renderer offsets
/// them by the camera's pan and converts to screen pixels.
pub fn layout_scene(scene: &mut GraphScene) {
    if scene.nodes.is_empty() {
        return;
    }
    let n = scene.nodes.len();

    // 1) Rank assignment.
    let mut rank: Vec<Option<usize>> = vec![None; n];
    for (i, node) in scene.nodes.iter().enumerate() {
        if let Some(r) = node.hints.rank {
            rank[i] = Some(r);
        }
    }
    let mut succs: Vec<Vec<NodeId>> = vec![Vec::new(); n];
    let mut preds: Vec<Vec<NodeId>> = vec![Vec::new(); n];
    for e in &scene.edges {
        succs[e.from.0].push(e.to);
        preds[e.to.0].push(e.from);
    }
    // BFS longest-path from node 0 over forward edges (skip back-edges
    // by id ordering as a heuristic).
    if rank[0].is_none() {
        rank[0] = Some(0);
    }
    let mut queue: std::collections::VecDeque<usize> =
        std::collections::VecDeque::from([0]);
    while let Some(i) = queue.pop_front() {
        let r = rank[i].unwrap_or(0);
        for &NodeId(j) in &succs[i] {
            if j <= i {
                continue;
            }
            let new = r + 1;
            if rank[j].map(|prev| new > prev).unwrap_or(true) {
                rank[j] = Some(new);
                queue.push_back(j);
            }
        }
    }
    let max_rank = rank.iter().filter_map(|r| *r).max().unwrap_or(0);
    for r in rank.iter_mut() {
        if r.is_none() {
            *r = Some(max_rank + 1);
        }
    }

    // Group by rank.
    let mut by_rank: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
    for (i, r) in rank.iter().enumerate() {
        by_rank.entry(r.unwrap()).or_default().push(NodeId(i));
    }
    let ranks: Vec<usize> = by_rank.keys().copied().collect();

    // 2) Barycenter sweeps.
    let mut pos: Vec<usize> = vec![0; n];
    for ids in by_rank.values() {
        for (i, &NodeId(id)) in ids.iter().enumerate() {
            pos[id] = i;
        }
    }
    let avg_position = |of_ids: &[NodeId], pos: &[usize]| -> f32 {
        if of_ids.is_empty() {
            return f32::INFINITY;
        }
        let sum: f32 = of_ids.iter().map(|nid| pos[nid.0] as f32).sum();
        sum / of_ids.len() as f32
    };
    for _ in 0..8 {
        let mut changed = false;
        for &r in ranks.iter().skip(1) {
            if let Some(ids) = by_rank.get_mut(&r) {
                let mut keyed: Vec<(f32, NodeId)> = ids
                    .iter()
                    .map(|&nid| (avg_position(&preds[nid.0], &pos), nid))
                    .collect();
                keyed.sort_by(|a, b| {
                    a.0.partial_cmp(&b.0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.1.0.cmp(&b.1.0))
                });
                let new_ids: Vec<NodeId> = keyed.iter().map(|(_, nid)| *nid).collect();
                if &new_ids != ids {
                    changed = true;
                    *ids = new_ids;
                    for (i, &NodeId(id)) in ids.iter().enumerate() {
                        pos[id] = i;
                    }
                }
            }
        }
        for &r in ranks.iter().rev().skip(1) {
            if let Some(ids) = by_rank.get_mut(&r) {
                let mut keyed: Vec<(f32, NodeId)> = ids
                    .iter()
                    .map(|&nid| (avg_position(&succs[nid.0], &pos), nid))
                    .collect();
                keyed.sort_by(|a, b| {
                    a.0.partial_cmp(&b.0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.1.0.cmp(&b.1.0))
                });
                let new_ids: Vec<NodeId> = keyed.iter().map(|(_, nid)| *nid).collect();
                if &new_ids != ids {
                    changed = true;
                    *ids = new_ids;
                    for (i, &NodeId(id)) in ids.iter().enumerate() {
                        pos[id] = i;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    // 3) X positions. For each rank, sort by barycenter order and
    // place left-to-right honouring each node's size_px.
    const RANK_GAP_PX: f32 = 60.;
    const COL_GAP_PX: f32 = 30.;
    let mut world_x: Vec<f32> = vec![0.; n];
    let mut world_y: Vec<f32> = vec![0.; n];
    let mut cursor_y_px = 0.0_f32;
    for (_r, ids) in &by_rank {
        let total_w_px: f32 = ids
            .iter()
            .map(|nid| scene.nodes[nid.0].hints.size_px.0)
            .sum::<f32>()
            + COL_GAP_PX * ((ids.len().saturating_sub(1)) as f32);
        let max_h_px = ids
            .iter()
            .map(|nid| scene.nodes[nid.0].hints.size_px.1)
            .fold(0.0_f32, f32::max);
        let mut x_cursor_px = -total_w_px / 2.;
        for &NodeId(id) in ids {
            let (w, _h) = scene.nodes[id].hints.size_px;
            // Convert px → world by dividing by WORLD_UNIT (zoom = 1).
            world_x[id] = x_cursor_px / WORLD_UNIT;
            world_y[id] = cursor_y_px / WORLD_UNIT;
            x_cursor_px += w + COL_GAP_PX;
        }
        cursor_y_px += max_h_px + RANK_GAP_PX;
    }

    // Commit positions only for nodes the caller hasn't pinned.
    for i in 0..n {
        if scene.positions[i].is_none() {
            scene.positions[i] = Some((world_x[i], world_y[i]));
        }
    }
}

// ---- Edge routing ---------------------------------------------------------

/// Result of routing one edge.
pub struct RoutedEdge {
    pub segments: Vec<EdgeSegment>,
    pub arrow_tip: (f32, f32),
    pub arrow_dir: ArrowHeadDir,
}

/// Per-node screen-space rectangle. Built once by the renderer
/// after applying the camera transform.
#[derive(Clone, Copy, Debug)]
pub struct NodeRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl NodeRect {
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.
    }
}

/// Route every edge given the placed-node rects. Edges entering /
/// exiting the same block are distributed across the block's top /
/// bottom edge in target-x order so they never cross each other at
/// the attach point.
pub fn route_edges(scene: &GraphScene, rects: &[NodeRect]) -> Vec<RoutedEdge> {
    let n = scene.nodes.len();
    // Group outgoing / incoming edges per node so we can sort them
    // by the other end's x for fan-in / fan-out distribution.
    let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut in_edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (ei, e) in scene.edges.iter().enumerate() {
        if e.from.0 < n {
            out_edges[e.from.0].push(ei);
        }
        if e.to.0 < n {
            in_edges[e.to.0].push(ei);
        }
    }
    let mut out_slot = vec![0usize; scene.edges.len()];
    let mut in_slot = vec![0usize; scene.edges.len()];
    for eids in out_edges.iter_mut() {
        eids.sort_by(|&a, &b| {
            let xa = rects.get(scene.edges[a].to.0).map(|r| r.cx()).unwrap_or(0.);
            let xb = rects.get(scene.edges[b].to.0).map(|r| r.cx()).unwrap_or(0.);
            xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal)
        });
        for (slot, &ei) in eids.iter().enumerate() {
            out_slot[ei] = slot;
        }
    }
    for eids in in_edges.iter_mut() {
        eids.sort_by(|&a, &b| {
            let xa = rects.get(scene.edges[a].from.0).map(|r| r.cx()).unwrap_or(0.);
            let xb = rects.get(scene.edges[b].from.0).map(|r| r.cx()).unwrap_or(0.);
            xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal)
        });
        for (slot, &ei) in eids.iter().enumerate() {
            in_slot[ei] = slot;
        }
    }
    let out_total: Vec<usize> = out_edges.iter().map(|v| v.len()).collect();
    let in_total: Vec<usize> = in_edges.iter().map(|v| v.len()).collect();

    // Trim the last segment by this much so the arrowhead's body
    // isn't covered by the line.
    const ARROW_TRIM_PX: f32 = 7.;

    let mut routed = Vec::with_capacity(scene.edges.len());
    for (ei, e) in scene.edges.iter().enumerate() {
        let Some(src) = rects.get(e.from.0).copied() else { continue };
        let Some(dst) = rects.get(e.to.0).copied() else { continue };
        let on = out_total[e.from.0].max(1);
        let in_n = in_total[e.to.0].max(1);
        let out_frac = (out_slot[ei] + 1) as f32 / (on + 1) as f32;
        let in_frac = (in_slot[ei] + 1) as f32 / (in_n + 1) as f32;
        let sx = src.x + src.w * out_frac;
        let sy = src.y + src.h;
        let tx = dst.x + dst.w * in_frac;
        let ty = dst.y;
        // Simple 3-segment Manhattan route: down to mid, across,
        // down to target. The earlier multi-rank / back-edge work
        // in render_cfg picked the horizontal y from the rank-gap
        // band — but that depends on rank geometry that's not yet
        // surfaced through this shared API. We'll re-introduce it
        // in a follow-up; this gets parity with the current DEX
        // call graph routing and gets the abstraction in.
        let mid_y = (sy + ty) / 2.;
        let mut segments = Vec::with_capacity(3);
        segments.push(EdgeSegment {
            x: sx,
            y: sy.min(mid_y),
            length: (mid_y - sy).abs(),
            horizontal: false,
        });
        segments.push(EdgeSegment {
            x: sx.min(tx),
            y: mid_y,
            length: (tx - sx).abs(),
            horizontal: true,
        });
        let final_y_top = mid_y.min(ty);
        let final_y_len = ((ty - mid_y).abs() - ARROW_TRIM_PX).max(0.);
        segments.push(EdgeSegment {
            x: tx,
            y: final_y_top,
            length: final_y_len,
            horizontal: false,
        });
        routed.push(RoutedEdge {
            segments,
            arrow_tip: (tx, ty),
            arrow_dir: ArrowHeadDir::Down,
        });
    }
    routed
}

// ---- Render plumbing -------------------------------------------------------

/// Per-node rect after applying the camera transform. The renderer
/// uses these for both block content (passed back to the content
/// callback) and edge routing.
pub fn compute_node_rects(scene: &GraphScene, camera: &GraphCamera) -> Vec<NodeRect> {
    let bounds = camera.viewport_bounds;
    let bounds_origin_x = bounds.origin.x.as_f32();
    let bounds_origin_y = bounds.origin.y.as_f32();
    let centre_x = bounds_origin_x + bounds.size.width.as_f32() / 2.;
    let centre_y = bounds_origin_y + bounds.size.height.as_f32() / 2.;
    let unit = camera.unit();
    scene
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let (wx, wy) = scene.positions[i].unwrap_or((0., 0.));
            let (w_px, h_px) = node.hints.size_px;
            let screen_x = centre_x + (wx - camera.pan_x) * unit;
            let screen_y = centre_y + (wy - camera.pan_y) * unit;
            NodeRect {
                x: screen_x - bounds_origin_x,
                y: screen_y - bounds_origin_y,
                w: w_px,
                h: h_px,
            }
        })
        .collect()
}

/// Bounds-unknown guard: viewport_bounds is `Bounds::default()`
/// until the canvas hook fires during the first paint. Use this
/// before culling so the first frame draws everything.
pub fn bounds_unknown(camera: &GraphCamera) -> bool {
    camera.viewport_bounds.size.width.as_f32() <= 0.
        || camera.viewport_bounds.size.height.as_f32() <= 0.
}
