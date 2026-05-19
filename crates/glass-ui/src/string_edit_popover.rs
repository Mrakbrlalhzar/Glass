//! Floating panel for editing a NUL-terminated string item
//! (typically `__cstring` or similar). Opens when the user
//! double-clicks anywhere inside a recognised string range in
//! the hex view.
//!
//! Layout: a 480 px card centred over the window with a backdrop
//! (clicking the backdrop cancels). Inside: header showing the
//! address and length, a multi-line wrapped TextInput
//! pre-populated with the current string, a small chip
//! reporting the used / max length budget, and an error chip
//! when the proposed string overflows the original allocation.
//!
//! Enter commits via `commit_hex_edit`; Esc / backdrop cancels
//! via `cancel_hex_edit`. Key events route through the existing
//! `hex_edit_handle_key` path so the popover doesn't need its
//! own key listener.

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};

use crate::{HexEditKind, HexEditState, Shell};

pub fn render(
    state: &HexEditState,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    debug_assert!(matches!(state.kind, HexEditKind::String));
    let current_len = state.input.text().as_bytes().len();
    let max_len = state.length;
    // Over budget: more bytes than the original allocation, OR
    // exactly the allocation with no NUL anywhere in the new
    // text (so no terminator).
    let no_nul_in_text = !state.input.text().as_bytes().contains(&0);
    let over_budget =
        current_len > max_len || (current_len == max_len && no_nul_in_text);
    let budget_chip = div()
        .text_xs()
        .text_color(if over_budget { crate::theme::current().hex.error_text.rgba() } else { dim })
        .child(SharedString::from(format!(
            "{} / {} bytes",
            current_len, max_len
        )));
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_sm()
                .text_color(fg)
                .child(SharedString::from(format!(
                    "Edit string at 0x{:x}",
                    state.address
                ))),
        )
        .child(budget_chip);
    let editor = div()
        .min_h(px(80.))
        .max_h(px(240.))
        .text_base()
        .child(state.input.render_multiline(
            fg,
            dim,
            "(empty string)",
            "Courier New",
            56,
        ));
    let footer_text = if let Some(err) = state.error.as_ref() {
        format!("Error: {err}")
    } else if over_budget {
        let need_to_shed = if current_len > max_len {
            current_len - max_len
        } else {
            // Exactly fills the slot but no NUL — need to free
            // one byte for the terminator.
            1
        };
        format!(
            "Too long — shorten by {} byte(s) to fit the original allocation.",
            need_to_shed
        )
    } else {
        "Enter saves, Esc cancels. Strings are NUL-padded to the original length.".to_string()
    };
    let footer_colour = if over_budget || state.error.is_some() {
        crate::theme::current().errors.highlight.rgba()
    } else {
        dim
    };
    let card = div()
        .id("string-edit-card")
        .w(px(480.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_md()
        .shadow_lg()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .occlude()
        .child(header)
        .child(editor)
        .child(
            div()
                .text_xs()
                .text_color(footer_colour)
                .child(SharedString::from(footer_text)),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        );
    div()
        .absolute()
        .inset_0()
        .bg(crate::theme::current().modals.overlay_light.rgba())
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                this.cancel_hex_edit(cx);
            }),
        )
        .child(div().mt(px(120.)).child(card))
        .into_any_element()
}
