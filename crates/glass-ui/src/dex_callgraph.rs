//! Per-tab state and pure graph-building helpers for the DEX call
//! graph view.
//!
//! The view's renderer + mouse handlers still live on `Shell` in
//! `lib.rs` because they're tightly coupled to tab routing and `cx`
//! captures. The data structure here, and the two functions that
//! mutate it (seed the root + its direct callees, expand a single
//! callee in place), have no UI dependencies.

use std::collections::HashMap;

use crate::graph;

/// Per-tab state for a DEX call-graph view. Owned by `Tab` in
/// `lib.rs`; rendered by `Shell::render_dex_callgraph`.
#[derive(Clone)]
pub struct DexCallGraphState {
    /// Shared camera (pan / zoom / drag / viewport bounds).
    pub camera: graph::GraphCamera,
    /// Shared scene of placed methods. Node indices double as the
    /// stable id for tracking expansion state.
    pub scene: graph::GraphScene,
    /// Parallel to `scene.nodes` — the JNI key of each placed method.
    /// We keep this on the side instead of as a generic node-payload
    /// type because the rest of `glass-ui` is closer to a script
    /// than a typed crate; let's not boil the ocean.
    pub keys: Vec<String>,
}

impl DexCallGraphState {
    pub fn new(pan_x: f32, pan_y: f32, zoom: f32) -> Self {
        Self {
            camera: graph::GraphCamera::new(pan_x, pan_y, zoom),
            scene: graph::GraphScene::default(),
            keys: Vec::new(),
        }
    }

    pub fn pan_x(&self) -> f32 {
        self.camera.pan_x
    }
    pub fn pan_y(&self) -> f32 {
        self.camera.pan_y
    }
    pub fn zoom(&self) -> f32 {
        self.camera.zoom
    }
}

const NODE_HINT_W: f32 = 220.;
const NODE_HINT_H: f32 = 32.;

/// Seed an empty `view` with the root method and its direct callees,
/// then run layout. No-op if the view is already populated.
pub fn seed_root(
    view: &mut DexCallGraphState,
    method_calls: &HashMap<String, Vec<String>>,
    class_jni: &str,
    method_decl: &str,
) {
    if !view.keys.is_empty() {
        return;
    }
    let root_key = format!("{class_jni}->{method_decl}");
    let hints = graph::NodeHints { size_px: (NODE_HINT_W, NODE_HINT_H), rank: None };
    let root_id = view.scene.add_node(
        root_key.as_str(),
        hints.clone(),
        graph::NodeTags { is_entry: true, ..Default::default() },
    );
    view.keys.push(root_key.clone());
    if let Some(callees) = method_calls.get(&root_key).cloned() {
        for callee in callees {
            let id = view.scene.add_node(
                callee.as_str(),
                hints.clone(),
                graph::NodeTags::default(),
            );
            view.keys.push(callee);
            view.scene.add_edge(
                root_id,
                id,
                graph::EdgeStyle::Solid,
                graph::EdgeKind::Call,
            );
        }
    }
    graph::layout_scene(&mut view.scene);
}

/// Add `key`'s un-placed callees to the scene and re-layout. Returns
/// `true` if any node was added (caller may want to persist state).
pub fn expand_callee(
    view: &mut DexCallGraphState,
    method_calls: &HashMap<String, Vec<String>>,
    key: &str,
) -> bool {
    let caller_id = match view.keys.iter().position(|k| k == key) {
        Some(i) => graph::NodeId(i),
        None => return false,
    };
    let placed: std::collections::HashSet<String> = view.keys.iter().cloned().collect();
    let Some(callees) = method_calls.get(key).cloned() else {
        return false;
    };
    let new_callees: Vec<String> =
        callees.into_iter().filter(|c| !placed.contains(c)).collect();
    if new_callees.is_empty() {
        return false;
    }
    let hints = graph::NodeHints { size_px: (NODE_HINT_W, NODE_HINT_H), rank: None };
    for callee in new_callees {
        let id = view.scene.add_node(
            callee.as_str(),
            hints.clone(),
            graph::NodeTags::default(),
        );
        view.keys.push(callee);
        view.scene.add_edge(
            caller_id,
            id,
            graph::EdgeStyle::Solid,
            graph::EdgeKind::Call,
        );
    }
    for pos in view.scene.positions.iter_mut() {
        *pos = None;
    }
    graph::layout_scene(&mut view.scene);
    true
}
