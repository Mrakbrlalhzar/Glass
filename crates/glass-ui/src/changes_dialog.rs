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
                        crate::edits::EditKind::Instruction => {
                            // 4-byte original: decode as AArch64 for the
                            // historical pretty-print. 2-byte (Thumb-1)
                            // originals don't decode that way — show the
                            // raw bytes so the dialog at least reads
                            // sensibly until the ARMv7 dialog decoder
                            // lands.
                            let old = if e.original_bytes.len() == 4 {
                                crate::shell_actions::decode_insn_pretty(
                                    &edit_bytes_4(&e.original_bytes),
                                    e.vaddr,
                                )
                            } else {
                                hex_bytes(&e.original_bytes)
                            };
                            (old, e.display.clone())
                        }
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

    let smali_entries: Vec<SmaliChangeView> = shell
        .bundle()
        .map(|b| {
            let mut out = Vec::new();
            for edit in b.smali_edits.entries() {
                let key = &edit.key;
                let Some(original) =
                    b.smali_classes.get(&(key.artifact.clone(), key.class_jni.clone()))
                else {
                    continue;
                };
                // Class-decl bucket — only if the class
                // declaration portion actually differs.
                if b.smali_edits.class_decl_differs(
                    &key.artifact,
                    &key.class_jni,
                    original,
                ) {
                    out.push(SmaliChangeView {
                        artifact: key.artifact.clone(),
                        class_jni: key.class_jni.clone(),
                        kind: SmaliChangeKind::ClassDecl,
                    });
                }
                for (name, sig) in b
                    .smali_edits
                    .edited_fields(&key.artifact, &key.class_jni, original)
                {
                    out.push(SmaliChangeView {
                        artifact: key.artifact.clone(),
                        class_jni: key.class_jni.clone(),
                        kind: SmaliChangeKind::Field { name, signature: sig },
                    });
                }
                for (name, sig) in b
                    .smali_edits
                    .edited_methods(&key.artifact, &key.class_jni, original)
                {
                    out.push(SmaliChangeView {
                        artifact: key.artifact.clone(),
                        class_jni: key.class_jni.clone(),
                        kind: SmaliChangeKind::Method { name, signature: sig },
                    });
                }
            }
            out
        })
        .unwrap_or_default();

    let total = entries.len() + smali_entries.len();
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
    if entries.is_empty() && smali_entries.is_empty() {
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
        for (i, row) in smali_entries.iter().enumerate() {
            list = list.child(render_smali_change_row(i, row, fg, dim, border, cx));
        }
    }

    let footer = build_footer(
        entries.len(),
        smali_entries.len(),
        confirm,
        fg,
        dim,
        border,
        cx,
    );

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

struct SmaliChangeView {
    artifact: glass_db::ArtifactId,
    class_jni: String,
    kind: SmaliChangeKind,
}

enum SmaliChangeKind {
    /// The class declaration portion (modifiers / super /
    /// implements / source / class-level annotations) was edited.
    ClassDecl,
    /// A specific field was edited.
    Field { name: String, signature: String },
    /// A specific method was edited.
    Method { name: String, signature: String },
}

fn render_smali_change_row(
    index: usize,
    row: &SmaliChangeView,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let display_class = row
        .class_jni
        .trim_start_matches('L')
        .trim_end_matches(';')
        .replace('/', ".");
    let (kind_label, member_label): (&'static str, String) = match &row.kind {
        SmaliChangeKind::ClassDecl => ("Class", display_class.clone()),
        SmaliChangeKind::Field { name, signature } => {
            ("Field", format!("{display_class}.{name}:{signature}"))
        }
        SmaliChangeKind::Method { name, signature } => {
            ("Method", format!("{display_class}.{name}{signature}"))
        }
    };
    // Click + revert closures need their own clones of the
    // identifiers — match arms capture by move.
    let artifact_for_click = row.artifact.clone();
    let class_for_click = row.class_jni.clone();
    let artifact_for_revert = row.artifact.clone();
    let class_for_revert = row.class_jni.clone();
    let kind_for_revert = match &row.kind {
        SmaliChangeKind::ClassDecl => SmaliChangeKind::ClassDecl,
        SmaliChangeKind::Field { name, signature } => SmaliChangeKind::Field {
            name: name.clone(),
            signature: signature.clone(),
        },
        SmaliChangeKind::Method { name, signature } => SmaliChangeKind::Method {
            name: name.clone(),
            signature: signature.clone(),
        },
    };
    let kind_for_click = match &row.kind {
        SmaliChangeKind::ClassDecl => SmaliChangeKind::ClassDecl,
        SmaliChangeKind::Field { name, signature } => SmaliChangeKind::Field {
            name: name.clone(),
            signature: signature.clone(),
        },
        SmaliChangeKind::Method { name, signature } => SmaliChangeKind::Method {
            name: name.clone(),
            signature: signature.clone(),
        },
    };
    div()
        .id(("changes-smali-row", index))
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
                .w(px(80.))
                .flex_shrink_0()
                .text_color(crate::theme::current().refs.dex_ref.rgba())
                .child(SharedString::from(kind_label)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(fg)
                .child(SharedString::from(member_label)),
        )
        .child(
            div()
                .w(px(140.))
                .flex_shrink_0()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from("smali edit")),
        )
        .child(
            div()
                .id(("changes-smali-revert", index))
                .px_2()
                .text_xs()
                .text_color(crate::theme::current().errors.severe.rgba())
                .cursor_pointer()
                .hover(|s| s.underline())
                .child(SharedString::from("Revert"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                        let _ = ev;
                        match &kind_for_revert {
                            SmaliChangeKind::ClassDecl => {
                                this.revert_smali_class_edit(
                                    artifact_for_revert.clone(),
                                    class_for_revert.clone(),
                                    cx,
                                );
                            }
                            SmaliChangeKind::Field { name, signature } => {
                                this.revert_smali_field_edit(
                                    artifact_for_revert.clone(),
                                    class_for_revert.clone(),
                                    name.clone(),
                                    signature.clone(),
                                    cx,
                                );
                            }
                            SmaliChangeKind::Method { name, signature } => {
                                this.revert_smali_method_edit(
                                    artifact_for_revert.clone(),
                                    class_for_revert.clone(),
                                    name.clone(),
                                    signature.clone(),
                                    cx,
                                );
                            }
                        }
                        cx.stop_propagation();
                    }),
                ),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, _ev, _w, cx| {
                match &kind_for_click {
                    SmaliChangeKind::ClassDecl => {
                        this.navigate_to_smali_class(
                            artifact_for_click.clone(),
                            class_for_click.clone(),
                            cx,
                        );
                    }
                    SmaliChangeKind::Field { name, signature } => {
                        this.navigate_to_smali_member(
                            artifact_for_click.clone(),
                            class_for_click.clone(),
                            crate::shell_actions::SmaliMemberKind::Field {
                                name: name.clone(),
                                signature: signature.clone(),
                            },
                            cx,
                        );
                    }
                    SmaliChangeKind::Method { name, signature } => {
                        this.navigate_to_smali_member(
                            artifact_for_click.clone(),
                            class_for_click.clone(),
                            crate::shell_actions::SmaliMemberKind::Method {
                                name: name.clone(),
                                signature: signature.clone(),
                            },
                            cx,
                        );
                    }
                }
            }),
        )
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
    byte_total: usize,
    smali_total: usize,
    confirm_abandon: bool,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let total = byte_total + smali_total;
    let export_disabled = total == 0;
    let abandon_disabled = total == 0;
    let export_label = format!(
        "Export {total} change{}…",
        if total == 1 { "" } else { "s" }
    );
    let export = div()
        .id("changes-export")
        .px_3()
        .py_1p5()
        .border_1()
        .border_color(border)
        .rounded_sm()
        .text_sm()
        .text_color(if export_disabled { dim } else { fg })
        .child(SharedString::from(export_label))
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
    let note: SharedString = SharedString::from("Esc closes");
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(note),
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
