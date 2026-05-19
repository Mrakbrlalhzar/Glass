//! Modal dialog listing every staged disasm edit.
//!
//! Opened from the toolbar's "N changes" button (or the ⌘E
//! chord). Each row shows the address, the original disasm, an
//! arrow, the new disasm, and a Revert button. Clicking a row
//! closes the dialog and navigates the active listing tab to
//! that address.
//!
//! The footer carries two actions: Export… (file-save dialog —
//! wired in a later checkpoint) and Abandon all (with a
//! confirmation step so a slip doesn't blow away the user's
//! edits).

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};

use crate::Shell;

pub fn render_changes_dialog(
    shell: &Shell,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let _ = accent;
    let entries = shell
        .bundle()
        .map(|b| {
            b.edits
                .entries()
                .into_iter()
                .map(|e| {
                    let (old_display, new_display) = match e.kind {
                        crate::edits::EditKind::Instruction => (
                            crate::shell_actions::decode_insn_pretty(
                                &edit_bytes_4(&e.original_bytes),
                                e.vaddr,
                            ),
                            e.display.clone(),
                        ),
                        crate::edits::EditKind::Bytes => (
                            hex_bytes(&e.original_bytes),
                            hex_bytes(&e.new_bytes),
                        ),
                        crate::edits::EditKind::String => (
                            format!("\"{}\"", c_string_preview(&e.original_bytes)),
                            format!("\"{}\"", c_string_preview(&e.new_bytes)),
                        ),
                    };
                    RowView {
                        artifact: e.artifact.clone(),
                        vaddr: e.vaddr,
                        original_disasm: old_display,
                        new_disasm: new_display,
                        new_bytes_preview: hex_bytes_preview(&e.new_bytes),
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let total = entries.len();
    let confirm = shell.changes_dialog_confirm_abandon;

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_lg()
                .text_color(fg)
                .child(SharedString::from(format!("{total} staged changes"))),
        )
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(
                    "Click a row to jump; Revert removes a single edit.",
                )),
        );

    let mut list = div()
        .flex()
        .flex_col()
        .gap_1()
        .max_h(px(420.))
        .child(div()); // anchor so we can chain children
    if entries.is_empty() {
        list = list.child(
            div()
                .py_8()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(dim)
                .child(SharedString::from(
                    "No staged edits. Double-click a disassembly row to start one.",
                )),
        );
    } else {
        for (i, row) in entries.iter().enumerate() {
            list = list.child(render_row(i, row, fg, dim, border, cx));
        }
    }

    let footer = build_footer(total, confirm, fg, dim, border, cx);

    let card = div()
        .id("changes-card")
        .w(px(820.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_md()
        .shadow_lg()
        .p_5()
        .flex()
        .flex_col()
        .gap_4()
        .occlude()
        .child(header)
        .child(list)
        .child(footer)
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
                this.close_changes_dialog(cx);
            }),
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}

struct RowView {
    artifact: glass_db::ArtifactId,
    vaddr: u64,
    original_disasm: String,
    new_disasm: String,
    new_bytes_preview: String,
}

/// Pad / truncate to 4 bytes so the existing instruction
/// decoder can render the original disasm of a 4-byte
/// Instruction edit's original_bytes.
fn edit_bytes_4(v: &[u8]) -> [u8; 4] {
    let mut out = [0u8; 4];
    let n = v.len().min(4);
    out[..n].copy_from_slice(&v[..n]);
    out
}

/// Hex preview ("aa bb cc dd …"), capped at 12 bytes so a
/// long string edit doesn't blow up the row.
fn hex_bytes_preview(bytes: &[u8]) -> String {
    const MAX: usize = 12;
    let mut s = String::new();
    for (i, b) in bytes.iter().take(MAX).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > MAX {
        s.push_str(" …");
    }
    s
}

fn hex_bytes(bytes: &[u8]) -> String {
    hex_bytes_preview(bytes)
}

/// Decode `bytes` as ASCII up to first NUL; replace unprintable
/// chars with `·`. Used for the Changes dialog's string preview.
fn c_string_preview(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .map(|b| if (0x20..0x7f).contains(b) { *b as char } else { '·' })
        .collect()
}

fn render_row(
    index: usize,
    row: &RowView,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let artifact_for_click = row.artifact.clone();
    let vaddr_for_click = row.vaddr;
    let artifact_for_revert = row.artifact.clone();
    let vaddr_for_revert = row.vaddr;
    let bytes_text = row.new_bytes_preview.clone();
    let body = div()
        .id(("changes-row", index))
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .py_1p5()
        .px_2()
        .border_1()
        .border_color(border)
        .rounded_sm()
        .cursor_pointer()
        .text_sm()
        .font_family("Courier New")
        .child(
            div()
                .w(px(140.))
                .flex_shrink_0()
                .text_color(crate::theme::current().refs.dex_ref.rgba())
                .child(format!("0x{:x}", row.vaddr)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(dim)
                .child(SharedString::from(row.original_disasm.clone())),
        )
        .child(
            div()
                .w(px(20.))
                .flex_shrink_0()
                .text_color(dim)
                .child(SharedString::from("→")),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(fg)
                .child(SharedString::from(row.new_disasm.clone())),
        )
        .child(
            div()
                .w(px(120.))
                .flex_shrink_0()
                .text_xs()
                .text_color(crate::theme::current().disasm.address.rgba())
                .child(SharedString::from(bytes_text)),
        )
        .child(
            div()
                .id(("changes-revert", index))
                .px_2()
                .text_xs()
                .text_color(crate::theme::current().errors.severe.rgba())
                .cursor_pointer()
                .hover(|s| s.underline())
                .child(SharedString::from("Revert"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                        // Stop propagation so the row's own
                        // click handler doesn't also fire and
                        // jump-then-close.
                        let _ = ev;
                        this.revert_disasm_edit(
                            artifact_for_revert.clone(),
                            vaddr_for_revert,
                            cx,
                        );
                        cx.stop_propagation();
                    }),
                ),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, _ev, _w, cx| {
                let artifact = artifact_for_click.clone();
                this.navigate_to_disasm_edit(artifact, vaddr_for_click, cx);
            }),
        );
    body
}

fn build_footer(
    total: usize,
    confirm_abandon: bool,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let export_disabled = total == 0;
    let abandon_disabled = total == 0;
    let export = div()
        .id("changes-export")
        .px_3()
        .py_1p5()
        .border_1()
        .border_color(border)
        .rounded_sm()
        .text_sm()
        .text_color(if export_disabled { dim } else { fg })
        .child(SharedString::from(format!("Export {total} changes…")))
        .when(!export_disabled, |d| {
            d.cursor_pointer().on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.export_patched_bundle(cx);
                }),
            )
        });
    let abandon_label = if confirm_abandon {
        "Click again to confirm"
    } else if total == 0 {
        "Abandon all"
    } else {
        "Abandon all…"
    };
    let abandon = div()
        .id("changes-abandon")
        .px_3()
        .py_1p5()
        .border_1()
        .border_color(if confirm_abandon { crate::theme::current().errors.severe.rgba() } else { border })
        .rounded_sm()
        .text_sm()
        .text_color(if abandon_disabled {
            dim
        } else if confirm_abandon {
            crate::theme::current().errors.severe.rgba()
        } else {
            fg
        })
        .child(SharedString::from(abandon_label.to_string()))
        .when(!abandon_disabled, |d| {
            d.cursor_pointer().on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    if this.changes_dialog_confirm_abandon {
                        this.abandon_all_disasm_edits(cx);
                    } else {
                        this.arm_abandon_confirm(cx);
                    }
                }),
            )
        });
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from("Esc closes")),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(abandon)
                .child(export),
        )
}
