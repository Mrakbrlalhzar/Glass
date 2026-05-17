//! Per-tab state and pure graph-building helpers for the DEX call
//! graph view.
//!
//! The view's renderer + mouse handlers still live on `Shell` in
//! `lib.rs` because they're tightly coupled to tab routing and `cx`
//! captures. The data structure here, and the two functions that
//! mutate it (seed the root + its direct callees, expand a single
//! callee in place), have no UI dependencies.

use std::collections::HashMap;

use gpui::SharedString;

use crate::{graph, LeafId};

/// Parsed presentation info for one method node. Computed once when
/// the node is added so per-frame rendering + sizing are cheap.
#[derive(Clone, Debug)]
pub struct DexNodeInfo {
    /// Simple class name, e.g. `Foo` from `Lcom/example/Foo;`.
    pub class_name: SharedString,
    /// Method name (no signature), e.g. `bar` from `bar(I)V`.
    pub method_name: SharedString,
    /// Signature including parens + return type, e.g. `(I)V`.
    pub signature: SharedString,
    /// Number of smali instructions in the method body — drives the
    /// node's height so big methods look big. `None` when the source
    /// body wasn't found (external / framework method).
    pub instruction_count: Option<usize>,
}

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
    pub keys: Vec<String>,
    /// Parallel to `scene.nodes` — presentation info per node.
    pub info: Vec<DexNodeInfo>,
}

impl DexCallGraphState {
    pub fn new(pan_x: f32, pan_y: f32, zoom: f32) -> Self {
        Self {
            camera: graph::GraphCamera::new(pan_x, pan_y, zoom),
            scene: graph::GraphScene::default(),
            keys: Vec::new(),
            info: Vec::new(),
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

// ---- Node sizing -----------------------------------------------------------
//
// CFG-style sizing: width grows with the longest displayed line,
// height with the instruction count (capped). Pixel metrics chosen
// to match the CFG renderer so both views look consistent.

const CHAR_PX: f32 = 7.;
const PADDING_PX_W: f32 = 28.;
const PADDING_PX_H: f32 = 18.;
const ROW_PX: f32 = 17.;
const MIN_NODE_PX_W: f32 = 180.;
const MAX_NODE_PX_W: f32 = 480.;
const MIN_NODE_PX_H: f32 = 48.;
const MAX_NODE_PX_H: f32 = 200.;

fn size_for(info: &DexNodeInfo) -> (f32, f32) {
    // Width: longest of (class header) vs (method + signature).
    let header_len = info.class_name.len();
    let method_sig_len = info.method_name.len() + info.signature.len();
    let footer_len = match info.instruction_count {
        Some(n) => format!("{n} insns").len(),
        None => "external".len(),
    };
    let longest = header_len.max(method_sig_len).max(footer_len);
    let w_px = ((longest as f32) * CHAR_PX + PADDING_PX_W)
        .clamp(MIN_NODE_PX_W, MAX_NODE_PX_W);

    // Height: header row + method row + footer row + extra rows
    // proportional to instruction count. ~1 row per 10 instructions
    // so methods scale visibly with size without becoming silly tall.
    let header_rows = 3.0; // class + method + footer
    let bonus_rows = info
        .instruction_count
        .map(|n| (n as f32 / 10.0).clamp(0., 9.0))
        .unwrap_or(0.);
    let h_px = (header_rows + bonus_rows) * ROW_PX + PADDING_PX_H;
    let h_px = h_px.clamp(MIN_NODE_PX_H, MAX_NODE_PX_H);
    (w_px, h_px)
}

// ---- Key parsing + instruction counting ------------------------------------

/// Parse a JNI method key `Lcom/example/Foo;->name(sig)ret` into its
/// presentational pieces.
fn parse_key(key: &str) -> (SharedString, SharedString, SharedString) {
    // Class name: between trailing `/` (or leading `L`) and the `;`.
    let class_jni = key.split("->").next().unwrap_or(key);
    let class_name = class_jni
        .trim_start_matches('L')
        .trim_end_matches(';')
        .rsplit('/')
        .next()
        .unwrap_or(class_jni)
        .to_string();

    // Method ref after `->`. Split on the opening paren.
    let method_part = key.split("->").nth(1).unwrap_or("");
    let (method_name, signature) = match method_part.find('(') {
        Some(idx) => (method_part[..idx].to_string(), method_part[idx..].to_string()),
        None => (method_part.to_string(), String::new()),
    };
    (
        SharedString::from(class_name),
        SharedString::from(method_name),
        SharedString::from(signature),
    )
}

/// Count instruction lines in a smali method body. Walks from
/// `start_line` to the next `.end method` (or EOF), counting lines
/// that look like instructions — non-blank, not a comment, not a
/// directive, not a label.
fn count_instructions(
    bodies: &[SharedString],
    method_lines: &HashMap<String, (LeafId, usize)>,
    key: &str,
) -> Option<usize> {
    let (LeafId(leaf), start) = method_lines.get(key)?.clone();
    let body = bodies.get(leaf)?;
    let mut n = 0usize;
    for line in body.lines().skip(start + 1) {
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        let first = t.as_bytes()[0];
        if first == b'.' {
            if t.starts_with(".end method") {
                break;
            }
            continue; // any other `.directive`
        }
        if first == b'#' || first == b':' {
            continue;
        }
        n += 1;
    }
    Some(n)
}

fn build_info(
    key: &str,
    bodies: &[SharedString],
    method_lines: &HashMap<String, (LeafId, usize)>,
) -> DexNodeInfo {
    let (class_name, method_name, signature) = parse_key(key);
    let instruction_count = count_instructions(bodies, method_lines, key);
    DexNodeInfo { class_name, method_name, signature, instruction_count }
}

// ---- Scene mutation --------------------------------------------------------

/// Seed an empty `view` with the root method and its direct callees,
/// then run layout. No-op if the view is already populated.
pub fn seed_root(
    view: &mut DexCallGraphState,
    method_calls: &HashMap<String, Vec<String>>,
    bodies: &[SharedString],
    method_lines: &HashMap<String, (LeafId, usize)>,
    class_jni: &str,
    method_decl: &str,
) {
    if !view.keys.is_empty() {
        return;
    }
    let root_key = format!("{class_jni}->{method_decl}");
    let root_info = build_info(&root_key, bodies, method_lines);
    let root_size = size_for(&root_info);
    let root_id = view.scene.add_node(
        root_key.as_str(),
        graph::NodeHints { size_px: root_size, rank: None, x_hint: None },
        graph::NodeTags { is_entry: true, ..Default::default() },
    );
    view.keys.push(root_key.clone());
    view.info.push(root_info);
    if let Some(callees) = method_calls.get(&root_key).cloned() {
        for callee in callees {
            let info = build_info(&callee, bodies, method_lines);
            let size = size_for(&info);
            let id = view.scene.add_node(
                callee.as_str(),
                graph::NodeHints { size_px: size, rank: None, x_hint: None },
                graph::NodeTags::default(),
            );
            view.keys.push(callee);
            view.info.push(info);
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
    bodies: &[SharedString],
    method_lines: &HashMap<String, (LeafId, usize)>,
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
    for callee in new_callees {
        let info = build_info(&callee, bodies, method_lines);
        let size = size_for(&info);
        let id = view.scene.add_node(
            callee.as_str(),
            graph::NodeHints { size_px: size, rank: None, x_hint: None },
            graph::NodeTags::default(),
        );
        view.keys.push(callee);
        view.info.push(info);
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
