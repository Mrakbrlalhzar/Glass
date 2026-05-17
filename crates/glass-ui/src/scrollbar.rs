//! Scrollbar helpers.
//!
//! Non-interactive (visual only) for now. Mouse-wheel scrolling
//! works because the underlying scrollable elements handle it;
//! clicking / dragging the thumb is a follow-up.

use gpui::{div, prelude::*, px, ListState, Pixels};

pub const SCROLLBAR_WIDTH: f32 = 10.;
pub const SCROLLBAR_MIN_THUMB: f32 = 24.;

pub fn list_scrollbar(
    state: &ListState,
    border: gpui::Rgba,
    thumb: gpui::Rgba,
) -> impl IntoElement {
    let max_offset = state.max_offset_for_scrollbar().y;
    let current = -state.scroll_px_offset_for_scrollbar().y;
    let viewport = state.viewport_bounds().size.height;
    track_and_thumb(viewport, max_offset, current, border, thumb)
}

/// Horizontal scrollbar driven by an explicit `h_offset` (managed by
/// the Shell). We don't know the viewport width without a layout-
/// time hook, so we approximate using the panel size we have at
/// render time. Good enough to give a visible position indicator.
pub fn horizontal_scrollbar_offset(
    h_offset: Pixels,
    content_width: Pixels,
    border: gpui::Rgba,
    thumb: gpui::Rgba,
) -> impl IntoElement {
    let viewport = px(content_width.as_f32() / 2.);
    let max_offset = (content_width - viewport).max(px(0.));
    let current = h_offset.clamp(px(0.), max_offset);

    if max_offset <= px(0.) || viewport <= px(0.) {
        return div()
            .h(px(SCROLLBAR_WIDTH))
            .w_full()
            .flex_shrink_0();
    }

    let total = viewport + max_offset;
    let thumb_w_raw = viewport.as_f32() * viewport.as_f32() / total.as_f32();
    let thumb_w = px(thumb_w_raw.max(SCROLLBAR_MIN_THUMB));
    let fraction = (current / max_offset).clamp(0., 1.);
    let track_space = (viewport - thumb_w).max(px(0.));
    let thumb_left = track_space * fraction;

    div()
        .h(px(SCROLLBAR_WIDTH))
        .w_full()
        .flex_shrink_0()
        .border_t_1()
        .border_color(border)
        .relative()
        .child(
            div()
                .absolute()
                .left(thumb_left)
                .top(px(2.))
                .h(px(SCROLLBAR_WIDTH - 4.))
                .w(thumb_w)
                .bg(thumb)
                .rounded_sm(),
        )
}

pub fn track_and_thumb(
    viewport: Pixels,
    max_offset: Pixels,
    current: Pixels,
    border: gpui::Rgba,
    thumb: gpui::Rgba,
) -> impl IntoElement {
    if max_offset <= px(0.) || viewport <= px(0.) {
        return div()
            .absolute()
            .top_0()
            .right_0()
            .w(px(SCROLLBAR_WIDTH))
            .h_full();
    }

    let total = viewport + max_offset;
    let thumb_h_raw = viewport.as_f32() * viewport.as_f32() / total.as_f32();
    let thumb_h = px(thumb_h_raw.max(SCROLLBAR_MIN_THUMB));
    let fraction = (current / max_offset).clamp(0., 1.);
    let track_space = (viewport - thumb_h).max(px(0.));
    let thumb_top = track_space * fraction;

    div()
        .absolute()
        .top_0()
        .right_0()
        .w(px(SCROLLBAR_WIDTH))
        .h_full()
        .border_l_1()
        .border_color(border)
        .child(
            div()
                .absolute()
                .top(thumb_top)
                .left(px(2.))
                .w(px(SCROLLBAR_WIDTH - 4.))
                .h(thumb_h)
                .bg(thumb)
                .rounded_sm(),
        )
}
