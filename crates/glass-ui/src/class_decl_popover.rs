//! Class-declaration editor popover.
//!
//! Opens when the user double-clicks the `.class` line in a smali
//! tab. Lets them change the visibility / modifiers (via
//! `modifier_picker`), super class, implemented interfaces, and
//! source-file hint. The class **name** is intentionally
//! read-only — renaming a class cascades into every reference
//! across all DEX files and the AndroidManifest, which is a
//! future "Refactor → Rename Class" command, not a local edit.
//!
//! Layout: a ~520 px card centred over the window with a
//! dimming backdrop (click cancels). Inside the card, one labelled
//! row per editable field. Each row's input validates
//! independently — the Save button is disabled until every input
//! is valid.
//!
//! Save runs the changes into a `SmaliClassEdit` staged in
//! `bundle.smali_edits`; the smali tab's `lines` cache is
//! invalidated so the next paint re-renders from the modified
//! class.
//!
//! Validation rules:
//!   * Super class:  `L<path>;` JNI object signature.
//!   * Interfaces:   each entry a `L<path>;` JNI object signature.
//!     Empty interface list is fine (means "no interfaces").
//!   * Source:       any text or empty.

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};
use smali::types::{Modifier, ObjectIdentifier, SmaliClass};

use crate::modifier_picker::{
    render_modifier_picker, set_visibility, toggle_modifier, ModifierSite, Visibility,
};
use crate::text_input::TextInput;
use crate::Shell;

/// Which text input is currently receiving keystrokes. Only one
/// field is focused at a time; Tab/Shift-Tab cycles forward and
/// back, and clicking an input row jumps focus directly to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassDeclFocus {
    Super,
    Interface(usize),
    Source,
}

/// Per-popover state. Lives on `Shell.class_decl_edit` while the
/// popover is open. Holds the in-progress form values; nothing is
/// committed until the user clicks Save.
pub struct ClassDeclEditState {
    /// Identifies which class is being edited. The artifact + jni
    /// pair is the key into `bundle.smali_edits`.
    pub artifact: glass_db::ArtifactId,
    pub class_jni: String,
    /// Display-only — the class name as a JNI signature, e.g.
    /// `Lcom/example/Foo;`. Read-only in the popover.
    pub name_display: SharedString,
    /// Live modifier set the picker drives.
    pub modifiers: Vec<Modifier>,
    /// Super-class JNI signature. Editable.
    pub super_class: TextInput,
    /// Interface list. Each row is one `L<path>;` JNI signature.
    pub interfaces: Vec<TextInput>,
    /// Optional `.source "Foo.java"` hint.
    pub source: TextInput,
    /// Which input currently receives keystrokes. Defaults to the
    /// super-class field so the user can start typing immediately
    /// after opening the popover.
    pub focus: ClassDeclFocus,
}

impl ClassDeclEditState {
    pub fn from_class(
        artifact: glass_db::ArtifactId,
        class_jni: String,
        class: &SmaliClass,
    ) -> Self {
        let name_display = SharedString::from(class.name.as_jni_type());
        let super_class = TextInput::from_text(class.super_class.as_jni_type());
        let interfaces = class
            .implements
            .iter()
            .map(|o| TextInput::from_text(o.as_jni_type()))
            .collect();
        let source = TextInput::from_text(class.source.clone().unwrap_or_default());
        Self {
            artifact,
            class_jni,
            name_display,
            modifiers: class.modifiers.clone(),
            super_class,
            interfaces,
            source,
            focus: ClassDeclFocus::Super,
        }
    }

    /// Mutable handle on the input that currently has keyboard
    /// focus. `None` if `focus` points at an interface row index
    /// that has since been removed.
    pub fn focused_input_mut(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            ClassDeclFocus::Super => Some(&mut self.super_class),
            ClassDeclFocus::Source => Some(&mut self.source),
            ClassDeclFocus::Interface(i) => self.interfaces.get_mut(i),
        }
    }

    /// Cycle focus to the next field. With `reverse=true` cycles
    /// backwards (Shift-Tab).
    pub fn cycle_focus(&mut self, reverse: bool) {
        let iface_n = self.interfaces.len();
        // Build the ordered list of valid focus targets, then move
        // by ±1 with wrap. Keeping the list flat keeps the cycle
        // intuitive: Super → iface[0] → … → iface[N-1] → Source.
        let mut order: Vec<ClassDeclFocus> = Vec::with_capacity(iface_n + 2);
        order.push(ClassDeclFocus::Super);
        for i in 0..iface_n {
            order.push(ClassDeclFocus::Interface(i));
        }
        order.push(ClassDeclFocus::Source);
        let cur = order
            .iter()
            .position(|f| *f == self.focus)
            .unwrap_or(0);
        let next = if reverse {
            (cur + order.len() - 1) % order.len()
        } else {
            (cur + 1) % order.len()
        };
        self.focus = order[next];
    }

    /// Validate the form. Returns `Ok(())` if every input parses,
    /// otherwise the first error message.
    pub fn validate(&self) -> Result<(), String> {
        validate_jni_object_sig(self.super_class.text())
            .map_err(|m| format!("Super class: {m}"))?;
        for (i, iface) in self.interfaces.iter().enumerate() {
            // An empty interface row is treated as "remove this entry"
            // at save time; skip validation here.
            if iface.text().trim().is_empty() {
                continue;
            }
            validate_jni_object_sig(iface.text())
                .map_err(|m| format!("Interface #{}: {m}", i + 1))?;
        }
        Ok(())
    }

    /// Build a modified `SmaliClass` from `original`, overlaying
    /// the form values. `original` is the unedited class — we
    /// preserve fields the popover doesn't touch (annotations,
    /// fields, methods, file_path).
    pub fn build_modified(&self, original: &SmaliClass) -> SmaliClass {
        let mut out = original.clone();
        out.modifiers = self.modifiers.clone();
        out.super_class = ObjectIdentifier::from_jni_type(self.super_class.text());
        out.implements = self
            .interfaces
            .iter()
            .filter_map(|i| {
                let s = i.text().trim();
                if s.is_empty() {
                    None
                } else {
                    Some(ObjectIdentifier::from_jni_type(s))
                }
            })
            .collect();
        let src = self.source.text().trim();
        out.source = if src.is_empty() { None } else { Some(src.to_string()) };
        out
    }
}

/// Recognise the lines that make up a class declaration — these
/// all open the class-decl popover when the user double-clicks
/// or hits Enter on them. Class-level annotations count too
/// (they're edited from the same popover), but recognising them
/// from a single line in isolation isn't possible: a `.annotation`
/// line could be class-level, field-level, or method-level. Use
/// [`class_decl_row_mask`] for the row-aware variant.
pub fn line_is_class_decl(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with(".class ")
        || t.starts_with(".super ")
        || t.starts_with(".implements ")
        || t.starts_with(".source ")
}

/// Per-row mask telling whether each row in `lines` belongs to
/// the class-declaration block — i.e. is one of:
///
///   * `.class` / `.super` / `.implements` / `.source` (the four
///     lines `line_is_class_decl` already catches in isolation), or
///   * any line inside a class-level `.annotation … .end annotation`
///     block (everything before the first `.field` or `.method`).
///
/// Once `.field` or `.method` appears we stop emitting `true`s —
/// from there on annotations are field-level or method-level and
/// belong to their own popovers.
///
/// Walks `lines` once; allocation is one `Vec<bool>` of the same
/// length. Cheap enough to recompute on every smali render.
pub fn class_decl_row_mask(lines: &[gpui::SharedString]) -> Vec<bool> {
    let mut out = vec![false; lines.len()];
    let mut in_class_scope = true;
    let mut in_annotation_block = false;
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim_start();
        // First `.field` or `.method` ends the class-decl scope —
        // after this point any annotations belong to a field or
        // method, not the class itself.
        if in_class_scope
            && (t.starts_with(".field ")
                || t.starts_with(".field\t")
                || t == ".field"
                || t.starts_with(".method ")
                || t.starts_with(".method\t")
                || t == ".method")
        {
            in_class_scope = false;
        }
        if !in_class_scope {
            continue;
        }
        // Track multi-line annotation blocks so the body rows
        // (between `.annotation …` and `.end annotation`) get
        // included, not just the opener.
        if in_annotation_block {
            out[i] = true;
            if t.starts_with(".end annotation") {
                in_annotation_block = false;
            }
            continue;
        }
        if t.starts_with(".annotation ") {
            out[i] = true;
            in_annotation_block = true;
            continue;
        }
        if line_is_class_decl(raw.as_ref()) {
            out[i] = true;
        }
    }
    out
}

/// Validate a JNI object signature: `L` + slash-separated identifier
/// path + `;`. We don't need to be exhaustive (the DEX writer will
/// catch any pathologies); this is just enough to keep typos out of
/// the edit registry.
fn validate_jni_object_sig(s: &str) -> Result<(), &'static str> {
    let s = s.trim();
    if !s.starts_with('L') {
        return Err("must start with `L`");
    }
    if !s.ends_with(';') {
        return Err("must end with `;`");
    }
    let inner = &s[1..s.len() - 1];
    if inner.is_empty() {
        return Err("class name is empty");
    }
    for part in inner.split('/') {
        if part.is_empty() {
            return Err("empty path component");
        }
        let first = part.chars().next().unwrap();
        if !first.is_ascii_alphabetic() && first != '_' && first != '$' {
            return Err("path component must start with a letter, `_` or `$`");
        }
        for ch in part.chars() {
            if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$') {
                return Err("path component contains invalid character");
            }
        }
    }
    Ok(())
}

/// Render the popover overlay. Caller embeds the returned element
/// in the root render tree above the rest of the chrome.
#[allow(clippy::too_many_arguments)]
pub fn render(
    state: &ClassDeclEditState,
    annotations: &[(String, String)],
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let validation = state.validate();
    let save_enabled = validation.is_ok();

    let super_row = labelled_input(
        "Super class",
        "class-decl-super",
        &state.super_class,
        fg,
        dim,
        accent,
        state.focus == ClassDeclFocus::Super,
        ClassDeclFocus::Super,
        cx,
    );
    let source_row = labelled_input(
        "Source",
        "class-decl-source",
        &state.source,
        fg,
        dim,
        accent,
        state.focus == ClassDeclFocus::Source,
        ClassDeclFocus::Source,
        cx,
    );
    let card = div()
        .id("class-decl-card")
        .w(px(720.))
        .max_h(px(640.))
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
        .child(header_row(fg, dim, &state.name_display))
        .child(modifiers_row(state, fg, dim, accent, cx))
        .child(super_row)
        .child(interfaces_section(state, fg, dim, accent, cx))
        .child(source_row)
        .child(annotations_section(state, annotations, fg, dim, accent, cx))
        .child(validation_row(validation.err(), dim))
        .child(footer_row(save_enabled, fg, dim, border, accent, cx))
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
                this.cancel_class_decl_edit(cx);
            }),
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}

fn header_row(fg: gpui::Rgba, dim: gpui::Rgba, name: &SharedString) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_sm()
                .text_color(fg)
                .child(SharedString::from("Class declaration")),
        )
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(name.clone()),
        )
}

fn modifiers_row(
    state: &ClassDeclEditState,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    // The picker takes plain `Fn(Visibility, &mut App)` /
    // `Fn(Modifier, &mut App)` callbacks. We need to reach the
    // Shell's `class_decl_edit` field, so route through the entity
    // handle by hand — `cx.entity()` gives a strong handle and
    // `cx.update_entity` lets us mutate Shell from a plain
    // `&mut App`.
    let entity = cx.entity();
    let entity_for_vis = entity.clone();
    let on_visibility = move |vis: Visibility, cx: &mut App| {
        cx.update_entity(&entity_for_vis, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.class_decl_edit.as_mut() {
                state.modifiers = set_visibility(&state.modifiers, vis);
                cx.notify();
            }
        });
    };
    let entity_for_toggle = entity;
    let on_toggle = move |m: Modifier, cx: &mut App| {
        cx.update_entity(&entity_for_toggle, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.class_decl_edit.as_mut() {
                state.modifiers = toggle_modifier(&state.modifiers, m.clone());
                cx.notify();
            }
        });
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from("Modifiers")),
        )
        .child(render_modifier_picker(
            "class-decl-mods",
            ModifierSite::Class,
            &state.modifiers,
            fg,
            dim,
            accent,
            on_visibility,
            on_toggle,
        ))
}

fn interfaces_section(
    state: &ClassDeclEditState,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(dim)
                        .child(SharedString::from("Implements")),
                )
                .child(
                    div()
                        .id("class-decl-add-iface")
                        .px_2()
                        .py_0p5()
                        .text_xs()
                        .text_color(fg)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(SharedString::from("+ add"))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|shell, _ev, _w, cx| {
                                if let Some(state) =
                                    shell.class_decl_edit.as_mut()
                                {
                                    state.interfaces.push(TextInput::new());
                                    cx.notify();
                                }
                            }),
                        ),
                ),
        );
    if state.interfaces.is_empty() {
        col = col.child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(SharedString::from("(none)")),
        );
    } else {
        for (i, iface) in state.interfaces.iter().enumerate() {
            let focused = state.focus == ClassDeclFocus::Interface(i);
            col = col.child(interface_row(i, iface, fg, dim, accent, focused, cx));
        }
    }
    col
}

fn interface_row(
    index: usize,
    input: &TextInput,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    focused: bool,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let row_id: &'static str = Box::leak(format!("class-decl-iface-{index}").into_boxed_str());
    let input_id: &'static str =
        Box::leak(format!("class-decl-iface-input-{index}").into_boxed_str());
    let remove_id: &'static str =
        Box::leak(format!("class-decl-iface-rm-{index}").into_boxed_str());
    let entity = cx.entity();
    let on_pos_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
        cx.update_entity(&entity, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.class_decl_edit.as_mut() {
                state.focus = ClassDeclFocus::Interface(index);
                if let Some(input) = state.focused_input_mut() {
                    input.set_cursor_pos(byte, shift);
                }
                cx.notify();
            }
        });
    };
    let inner = input.render_clickable(input_id, fg, dim, "L…;", "Courier New", on_pos_click);
    div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_x_hidden()
                .border_1()
                // Focus ring: highlight the input with the accent
                // border when this row currently receives keystrokes.
                .border_color(if focused { accent } else { gpui::rgba(0x00000000) })
                .rounded_sm()
                .child(inner),
        )
        .child(
            div()
                .id(remove_id)
                .text_xs()
                .text_color(dim)
                .cursor_pointer()
                .hover(|s| s.text_color(crate::theme::current().errors.severe.rgba()))
                .child(SharedString::from("×"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |shell, _ev, _w, cx| {
                        if let Some(state) = shell.class_decl_edit.as_mut() {
                            if index < state.interfaces.len() {
                                state.interfaces.remove(index);
                                // Re-anchor focus if the removed
                                // row was focused / shifted later
                                // rows' indices.
                                state.focus = match state.focus {
                                    ClassDeclFocus::Interface(i) if i == index => {
                                        ClassDeclFocus::Super
                                    }
                                    ClassDeclFocus::Interface(i) if i > index => {
                                        ClassDeclFocus::Interface(i - 1)
                                    }
                                    other => other,
                                };
                                cx.notify();
                            }
                        }
                    }),
                ),
        )
}

/// Read-and-render the annotations attached to this class. Each
/// row shows `<vis> <type-jni>` with Edit / × controls. Adding
/// opens the editor with `index = None`. The editor reads/writes
/// through the bundle's smali_edits so changes survive popover
/// close/reopen.
fn annotations_section(
    state: &ClassDeclEditState,
    summaries: &[(String, String)],
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let artifact = state.artifact.clone();
    let class_jni = state.class_jni.clone();
    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(dim)
                        .child(SharedString::from("Annotations")),
                )
                .child({
                    let artifact = artifact.clone();
                    let class_jni = class_jni.clone();
                    div()
                        .id("class-decl-add-annotation")
                        .px_2()
                        .py_0p5()
                        .text_xs()
                        .text_color(fg)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(SharedString::from("+ add"))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |shell, _ev, _w, cx| {
                                shell.open_class_annotation_editor(
                                    artifact.clone(),
                                    class_jni.clone(),
                                    None,
                                    cx,
                                );
                            }),
                        )
                }),
        );
    if summaries.is_empty() {
        col = col.child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(SharedString::from("(none)")),
        );
    } else {
        for (i, (vis, jni)) in summaries.iter().enumerate() {
            col = col.child(annotation_row(
                i, vis, jni, artifact.clone(), class_jni.clone(), fg, dim, accent, cx,
            ));
        }
    }
    col
}

#[allow(clippy::too_many_arguments)]
fn annotation_row(
    index: usize,
    vis: &str,
    jni: &str,
    artifact: glass_db::ArtifactId,
    class_jni: String,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    _accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let row_id: &'static str =
        Box::leak(format!("class-decl-annotation-{index}").into_boxed_str());
    let edit_id: &'static str =
        Box::leak(format!("class-decl-annotation-edit-{index}").into_boxed_str());
    let rm_id: &'static str =
        Box::leak(format!("class-decl-annotation-rm-{index}").into_boxed_str());
    let summary = format!("{vis} {jni}");
    let edit_artifact = artifact.clone();
    let edit_class = class_jni.clone();
    let rm_artifact = artifact;
    let rm_class = class_jni;
    div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_xs()
                .text_color(fg)
                .font_family("Courier New")
                .child(SharedString::from(summary)),
        )
        .child(
            div()
                .id(edit_id)
                .px_2()
                .py_0p5()
                .text_xs()
                .text_color(fg)
                .cursor_pointer()
                .hover(|s| s.underline())
                .child(SharedString::from("Edit…"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |shell, _ev, _w, cx| {
                        shell.open_class_annotation_editor(
                            edit_artifact.clone(),
                            edit_class.clone(),
                            Some(index),
                            cx,
                        );
                    }),
                ),
        )
        .child(
            div()
                .id(rm_id)
                .text_xs()
                .text_color(dim)
                .cursor_pointer()
                .hover(|s| s.text_color(crate::theme::current().errors.severe.rgba()))
                .child(SharedString::from("×"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |shell, _ev, _w, cx| {
                        shell.remove_class_annotation(
                            rm_artifact.clone(),
                            rm_class.clone(),
                            index,
                            cx,
                        );
                    }),
                ),
        )
}

/// Single labelled input row — used by Super class and Source.
/// `focus_target` is the focus the row should claim on click;
/// `focused` is whether the row currently has focus (for the
/// accent-coloured border ring).
#[allow(clippy::too_many_arguments)]
fn labelled_input(
    label: &'static str,
    id: &'static str,
    input: &TextInput,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    focused: bool,
    focus_target: ClassDeclFocus,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    // Pre-render the char-level click overlay. Each char click
    // focuses this row's field and positions the caret to the
    // clicked byte offset (shift extends the selection).
    let entity = cx.entity();
    let on_click_target = focus_target;
    let on_pos_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
        cx.update_entity(&entity, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.class_decl_edit.as_mut() {
                state.focus = on_click_target;
                if let Some(input) = state.focused_input_mut() {
                    input.set_cursor_pos(byte, shift);
                }
                cx.notify();
            }
        });
    };
    let inner = input.render_clickable(id, fg, dim, "", "Courier New", on_pos_click);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(label)),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .overflow_x_hidden()
                .border_1()
                .border_color(if focused { accent } else { gpui::rgba(0x00000000) })
                .rounded_sm()
                .child(inner),
        )
}

fn validation_row(err: Option<String>, dim: gpui::Rgba) -> gpui::Div {
    match err {
        Some(msg) => div().text_xs().text_color(
            crate::theme::current().errors.highlight.rgba(),
        ).child(SharedString::from(msg)),
        None => div().text_xs().text_color(dim).child(SharedString::from(
            "Enter saves · Esc cancels.",
        )),
    }
}

fn footer_row(
    save_enabled: bool,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let save = div()
        .id("class-decl-save")
        .px_3()
        .py_1p5()
        .rounded_sm()
        .border_1()
        .border_color(if save_enabled { accent } else { border })
        .text_sm()
        .text_color(if save_enabled { fg } else { dim })
        .child(SharedString::from("Save"))
        .when(save_enabled, |d| {
            d.cursor_pointer().on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|shell, _ev, _w, cx| {
                    shell.commit_class_decl_edit(cx);
                }),
            )
        });
    let cancel = div()
        .id("class-decl-cancel")
        .px_3()
        .py_1p5()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .text_sm()
        .text_color(fg)
        .cursor_pointer()
        .child(SharedString::from("Cancel"))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                shell.cancel_class_decl_edit(cx);
            }),
        );
    div()
        .flex()
        .flex_row()
        .gap_2()
        .justify_end()
        .child(cancel)
        .child(save)
}

/// Forward a synthetic keystroke (key name only, no modifiers) to
/// the focused input. Used by the action handlers that intercept
/// `left` / `right` / etc. before they reach `on_key_down`.
pub fn handle_named_key(shell: &mut Shell, key: &str, cx: &mut Context<Shell>) {
    if let Some(state) = shell.class_decl_edit.as_mut() {
        if let Some(input) = state.focused_input_mut() {
            input.handle_key(key, false, false, false, None, cx);
            cx.notify();
        }
    }
}

/// Route a keystroke through the popover. Called from the Shell-
/// level `on_key_down` listener when `class_decl_edit` is `Some`.
/// Handles Enter / Esc / Tab at the popover level and forwards
/// everything else to the focused input.
pub fn handle_key(shell: &mut Shell, ks: &gpui::Keystroke, cx: &mut Context<Shell>) {
    let key = ks.key.as_str();
    if key == "escape" {
        shell.cancel_class_decl_edit(cx);
        return;
    }
    if key == "enter" {
        shell.commit_class_decl_edit(cx);
        return;
    }
    if key == "tab" {
        if let Some(state) = shell.class_decl_edit.as_mut() {
            state.cycle_focus(ks.modifiers.shift);
            cx.notify();
        }
        return;
    }
    if let Some(state) = shell.class_decl_edit.as_mut() {
        if let Some(input) = state.focused_input_mut() {
            let mutated = input.handle_key(
                key,
                ks.modifiers.shift,
                ks.modifiers.platform || ks.modifiers.control,
                ks.modifiers.alt,
                ks.key_char.as_deref(),
                cx,
            );
            if mutated {
                cx.notify();
            } else {
                // Cursor/selection moves still need a repaint to
                // update the caret position.
                cx.notify();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_well_formed_jni() {
        assert!(validate_jni_object_sig("Ljava/lang/Object;").is_ok());
        assert!(validate_jni_object_sig("Lcom/example/Foo$Inner;").is_ok());
        assert!(validate_jni_object_sig("La/b/c/D_e;").is_ok());
    }

    #[test]
    fn row_mask_covers_class_decl_lines() {
        let lines: Vec<SharedString> = [
            ".class public Lcom/Foo;",
            ".super Ljava/lang/Object;",
            ".source \"Foo.java\"",
            ".implements Ljava/io/Serializable;",
            "",
            ".field private count:I",
            "",
            ".method public foo()V",
            "    return-void",
            ".end method",
        ]
        .into_iter()
        .map(SharedString::from)
        .collect();
        let mask = class_decl_row_mask(&lines);
        // Class-decl lines tagged, field/method untagged.
        assert!(mask[0] && mask[1] && mask[2] && mask[3]);
        assert!(!mask[5]); // .field
        assert!(!mask[7]); // .method
        assert!(!mask[8]); // method body
    }

    #[test]
    fn row_mask_includes_class_level_annotation_block() {
        let lines: Vec<SharedString> = [
            ".class public Lcom/Foo;",
            ".super Ljava/lang/Object;",
            ".annotation runtime Ldagger/Module;",
            "    includes = {",
            "        Lfoo/Bar;",
            "    }",
            ".end annotation",
            ".field private count:I",
            ".annotation runtime Ldagger/Provides;",
            ".end annotation",
        ]
        .into_iter()
        .map(SharedString::from)
        .collect();
        let mask = class_decl_row_mask(&lines);
        for i in 0..=6 {
            assert!(mask[i], "class-level row {i} should be tagged");
        }
        // Field-level annotation block after `.field` is NOT
        // class-decl scope.
        assert!(!mask[7]); // .field
        assert!(!mask[8]); // .annotation on field
        assert!(!mask[9]); // .end annotation on field
    }

    #[test]
    fn rejects_malformed_jni() {
        assert!(validate_jni_object_sig("java/lang/Object").is_err());
        assert!(validate_jni_object_sig("Ljava/lang/Object").is_err());
        assert!(validate_jni_object_sig("Ljava//Object;").is_err());
        assert!(validate_jni_object_sig("L;").is_err());
        assert!(validate_jni_object_sig("L1stClass;").is_err());
        assert!(validate_jni_object_sig("Lcom/foo-bar;").is_err());
    }
}
