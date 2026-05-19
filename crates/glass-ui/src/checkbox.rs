//! Reusable checkbox widget.
//!
//! Pure renderer — takes the current state + a click callback,
//! returns a `Div`. Caller stores the `bool` somewhere and
//! flips it inside the callback. Intentionally tiny: a 12 px
//! box on the left, label text on the right, both in a clickable
//! row.
//!
//! Visual states:
//! - Unchecked: hollow box (border only).
//! - Checked: filled box with a tick glyph.
//!
//! Colours are caller-supplied so this can sit on light or dark
//! chrome.

use gpui::{div, px, rgb, App, InteractiveElement, ParentElement, Rgba, SharedString, Styled};

/// Render a checkbox row. `id` must be unique within the
/// rendered tree (used by gpui for click hit-testing).
/// `on_click` runs on left-click; the caller is expected to flip
/// the checked state themselves.
pub fn checkbox<F>(
    id: &'static str,
    label: impl Into<SharedString>,
    checked: bool,
    fg: Rgba,
    dim: Rgba,
    accent: Rgba,
    on_click: F,
) -> gpui::Stateful<gpui::Div>
where
    F: Fn(&mut App) + 'static,
{
    let box_bg: Rgba = if checked { accent } else { rgb(0x00000000) };
    let box_border: Rgba = if checked { accent } else { dim };
    let tick = if checked { "✓" } else { "" };
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .child(
            div()
                .w(px(12.))
                .h(px(12.))
                .border_1()
                .border_color(box_border)
                .bg(box_bg)
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(fg)
                .child(SharedString::from(tick)),
        )
        .child(
            div()
                .text_xs()
                .text_color(fg)
                .child(label.into()),
        )
        .on_mouse_down(gpui::MouseButton::Left, move |_ev, _w, cx: &mut App| {
            on_click(cx);
        })
        .child(div().w(px(0.)).text_color(dim)) // silence unused-binding
}
