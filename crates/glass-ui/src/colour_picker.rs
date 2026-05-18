//! Tiny colour swatch popover used by the listing context menu.
//!
//! Anchored at the previous context-menu position so it appears
//! under the user's cursor. Eight preset swatches plus a "clear"
//! tile. Click commits the colour through `Shell::pick_colour`
//! (which writes via glass-api and refreshes the in-memory index).

use gpui::{div, prelude::*, px, App, Context, Pixels};

use crate::{ColourPickerState, Shell};

/// Preset swatches. Alpha is full opacity (`ff`) — the listing
/// renderer dims the alpha on the row background, so the user
/// picks the colour, not the opacity. Order matches a typical
/// reverser's mental model (good / suspect / hostile / cool /
/// data / neutral).
const SWATCHES: &[(u32, &str)] = &[
    (0xff5252ff, "red"),
    (0xff8800ff, "orange"),
    (0xffd54fff, "yellow"),
    (0x66bb6aff, "green"),
    (0x4f7cffff, "blue"),
    (0xab47bcff, "purple"),
    (0x90a4aeff, "grey"),
    (0xefefefff, "white"),
];

pub fn render_colour_picker(
    state: &ColourPickerState,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let weak = cx.entity().downgrade();
    let weak_backdrop = weak.clone();

    // Anchor — clip below window bottom so the popover stays
    // visible when right-click lands near the screen edge.
    let position = state.position;
    let current = state.current;
    let mut chips = div().flex().flex_row().gap_2();

    for (i, (rgba, _name)) in SWATCHES.iter().enumerate() {
        let rgba = *rgba;
        let is_selected = current == Some(rgba);
        let weak = weak.clone();
        let chip = div()
            .id(("colour-chip", i))
            .w(px(20.))
            .h(px(20.))
            .rounded_sm()
            .bg(gpui::rgba(rgba))
            .border_2()
            .border_color(if is_selected { fg } else { border })
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |_ev, _w, cx: &mut App| {
                    cx.stop_propagation();
                    if let Some(entity) = weak.upgrade() {
                        cx.update_entity(&entity, |shell, cx| {
                            shell.pick_colour(Some(rgba), cx);
                        });
                    }
                },
            );
        chips = chips.child(chip);
    }
    // Clear tile — diagonal stripe pattern would be nice but is
    // overkill; render a small ⊘ glyph instead.
    let weak = weak.clone();
    let clear = div()
        .id("colour-clear")
        .w(px(20.))
        .h(px(20.))
        .rounded_sm()
        .border_1()
        .border_color(border)
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(fg)
        .cursor_pointer()
        .child("⊘")
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
                if let Some(entity) = weak.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.pick_colour(None, cx);
                    });
                }
            },
        );

    let popover = div()
        .absolute()
        .left(position.x)
        .top(position.y)
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_sm()
        .p_2()
        .shadow_md()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .occlude()
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        )
        .child(chips)
        .child(clear);

    // Dismiss backdrop — clicking outside closes without picking.
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |_ev, _w, cx: &mut App| {
                if let Some(entity) = weak_backdrop.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.close_colour_picker(cx);
                    });
                }
            },
        )
        .child(popover)
}

// Pixels is re-exported here to keep the call-site brief.
#[allow(dead_code)]
fn _pixels_marker(_: Pixels) {}
