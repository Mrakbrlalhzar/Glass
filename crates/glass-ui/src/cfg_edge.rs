//! CFG edge segments + arrowheads.
//!
//! Local renderer used by the legacy `Shell::render_cfg`. The
//! shared graph module (`graph.rs`) has its own copies that the DEX
//! call-graph view uses; eventually the CFG view should migrate to
//! those, at which point this module disappears.

use gpui::{div, prelude::*, px};

const CFG_EDGE_THICKNESS: f32 = 2.;
const CFG_EDGE_COLOR_SOLID: u32 = 0x9aa3b3;
const CFG_EDGE_COLOR_DOTTED: u32 = 0x6e7382;
const CFG_DOT_LEN: f32 = 4.;
const CFG_DOT_GAP: f32 = 3.;

/// A horizontal or vertical line segment in screen-local pixel
/// coordinates. The renderer uses these for both straight strokes
/// (1 px-thick rectangles) and dotted segments (a row of small
/// rectangles).
pub struct EdgeSegment {
    pub x: f32,
    pub y: f32,
    /// Length along the segment's axis.
    pub length: f32,
    /// True for horizontal, false for vertical.
    pub horizontal: bool,
}

pub fn render_edge_segment(seg: EdgeSegment, dotted: bool) -> gpui::Div {
    if dotted {
        let mut wrapper = div()
            .absolute()
            .left(px(seg.x - CFG_EDGE_THICKNESS / 2.))
            .top(px(seg.y - CFG_EDGE_THICKNESS / 2.));
        let stride = CFG_DOT_LEN + CFG_DOT_GAP;
        let mut pos = 0.0_f32;
        let length = seg.length;
        let colour = gpui::rgba((CFG_EDGE_COLOR_DOTTED << 8) | 0xee);
        if seg.horizontal {
            wrapper = wrapper
                .w(px(length + CFG_EDGE_THICKNESS))
                .h(px(CFG_EDGE_THICKNESS));
            while pos < length {
                let len = CFG_DOT_LEN.min(length - pos);
                wrapper = wrapper.child(
                    div()
                        .absolute()
                        .left(px(pos))
                        .top(px(0.))
                        .w(px(len))
                        .h(px(CFG_EDGE_THICKNESS))
                        .bg(colour),
                );
                pos += stride;
            }
        } else {
            wrapper = wrapper
                .w(px(CFG_EDGE_THICKNESS))
                .h(px(length + CFG_EDGE_THICKNESS));
            while pos < length {
                let len = CFG_DOT_LEN.min(length - pos);
                wrapper = wrapper.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px(pos))
                        .w(px(CFG_EDGE_THICKNESS))
                        .h(px(len))
                        .bg(colour),
                );
                pos += stride;
            }
        }
        wrapper
    } else {
        let colour = gpui::rgba((CFG_EDGE_COLOR_SOLID << 8) | 0xee);
        if seg.horizontal {
            div()
                .absolute()
                .left(px(seg.x))
                .top(px(seg.y - CFG_EDGE_THICKNESS / 2.))
                .w(px(seg.length))
                .h(px(CFG_EDGE_THICKNESS))
                .bg(colour)
        } else {
            div()
                .absolute()
                .left(px(seg.x - CFG_EDGE_THICKNESS / 2.))
                .top(px(seg.y))
                .w(px(CFG_EDGE_THICKNESS))
                .h(px(seg.length))
                .bg(colour)
        }
    }
}

/// Cardinal direction the arrowhead points in.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // Up is unused today — kept for symmetry with Down.
pub enum ArrowHeadDir {
    Down,
    Up,
    Left,
    Right,
}

/// Filled triangular arrowhead anchored at the tip `(tip_x, tip_y)`.
pub fn render_edge_arrowhead(
    tip_x: f32,
    tip_y: f32,
    dir: ArrowHeadDir,
) -> gpui::Div {
    const HEAD_HALF: f32 = 5.;
    const HEAD_LEN: f32 = 7.;
    let colour = gpui::rgba((CFG_EDGE_COLOR_SOLID << 8) | 0xee);

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
