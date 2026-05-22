//! Field editor popover.
//!
//! Opens when the user double-clicks (or hits Enter on) a `.field`
//! line in a smali tab. Lets them edit the field's name, type
//! signature, initial value, and modifiers (via
//! `modifier_picker::ModifierSite::Field`). Field-level annotations
//! are read-only here; they get their own editor in M1.7.
//!
//! Layout mirrors the class-decl popover: a 720 px card centred
//! over the window with a dimming backdrop (click cancels). Save
//! stages the edit into the parent `SmaliClass` in
//! `bundle.smali_edits` — the registry is class-keyed, so editing
//! a field means cloning the class, swapping that one field, and
//! re-inserting.
//!
//! Field identity inside the class is `(original_name,
//! original_signature_jni)`. Both are user-editable in the
//! popover, so we hold onto the originals at open time and use
//! them to locate the slot to replace at commit. New fields aren't
//! a supported operation yet (M1.4 covers editing; adding gets
//! folded into the class-level Add/Delete UX later).
//!
//! Validation rules:
//!
//! * Name:    non-empty, Java identifier shape
//!   (`[A-Za-z_$][A-Za-z0-9_$]*`).
//! * Type:    any valid JNI type signature (primitive,
//!   `L<path>;`, or `[`-prefixed array of same). Generic /
//!   wildcard forms aren't allowed in field type slots so we
//!   don't pretend to support them.
//! * Initial: free-form; we don't try to parse smali literals
//!   here. Empty means "no initialiser".

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};
use smali::types::{Modifier, SmaliClass, SmaliField, TypeSignature};

use crate::modifier_picker::{
    render_modifier_picker, set_visibility, toggle_modifier, ModifierSite, Visibility,
};
use crate::text_input::TextInput;
use crate::Shell;

/// Which text input is currently receiving keystrokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldFocus {
    Name,
    Type,
    InitialValue,
}

/// Per-popover state. Lives on `Shell.field_edit` while open.
pub struct FieldEditState {
    pub artifact: glass_db::ArtifactId,
    pub class_jni: String,
    /// Display-only — dotted Java class name, for the header.
    pub class_name_display: SharedString,
    /// Original `(name, signature_jni)` — used to locate the field
    /// inside the class at commit, since both are user-editable.
    pub original_name: String,
    pub original_signature_jni: String,
    /// Live modifier set the picker drives.
    pub modifiers: Vec<Modifier>,
    pub name: TextInput,
    pub signature: TextInput,
    pub initial_value: TextInput,
    pub focus: FieldFocus,
}

impl FieldEditState {
    /// Build state from a `SmaliField` belonging to `class`. Caller
    /// has already resolved `(artifact, class_jni, field)` from the
    /// active tab and selected row.
    pub fn from_field(
        artifact: glass_db::ArtifactId,
        class_jni: String,
        class: &SmaliClass,
        field: &SmaliField,
    ) -> Self {
        Self {
            artifact,
            class_jni,
            class_name_display: SharedString::from(class.name.as_java_type()),
            original_name: field.name.clone(),
            original_signature_jni: field.signature.to_jni(),
            modifiers: field.modifiers.clone(),
            name: TextInput::from_text(field.name.clone()),
            signature: TextInput::from_text(field.signature.to_jni()),
            initial_value: TextInput::from_text(
                field.initial_value.clone().unwrap_or_default(),
            ),
            focus: FieldFocus::Name,
        }
    }

    pub fn focused_input_mut(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            FieldFocus::Name => Some(&mut self.name),
            FieldFocus::Type => Some(&mut self.signature),
            FieldFocus::InitialValue => Some(&mut self.initial_value),
        }
    }

    pub fn cycle_focus(&mut self, reverse: bool) {
        let order = [FieldFocus::Name, FieldFocus::Type, FieldFocus::InitialValue];
        let cur = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if reverse {
            (cur + order.len() - 1) % order.len()
        } else {
            (cur + 1) % order.len()
        };
        self.focus = order[next];
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_field_name(self.name.text())
            .map_err(|m| format!("Name: {m}"))?;
        validate_field_type(self.signature.text())
            .map_err(|m| format!("Type: {m}"))?;
        Ok(())
    }

    /// Splice the form values into `original_class`, replacing the
    /// field identified by `(original_name, original_signature_jni)`.
    /// Returns `None` if no matching field is found — caller should
    /// keep the popover open and report.
    pub fn build_modified(&self, original_class: &SmaliClass) -> Option<SmaliClass> {
        let mut out = original_class.clone();
        let idx = out.fields.iter().position(|f| {
            f.name == self.original_name
                && f.signature.to_jni() == self.original_signature_jni
        })?;
        let initial = self.initial_value.text().trim().to_string();
        let new_field = SmaliField {
            name: self.name.text().trim().to_string(),
            modifiers: self.modifiers.clone(),
            signature: TypeSignature::from_jni(self.signature.text().trim()),
            initial_value: if initial.is_empty() { None } else { Some(initial) },
            annotations: out.fields[idx].annotations.clone(),
        };
        out.fields[idx] = new_field;
        Some(out)
    }
}

/// Recognise the rows the field popover opens on. Smali fields are
/// declared on a single line beginning with `.field`. Multi-line
/// fields (those with annotations) still have the `.field` header
/// as their first line — that's the one we target.
pub fn line_is_field_decl(line: &str) -> bool {
    line.trim_start().starts_with(".field ")
}

/// Validate a Java identifier: non-empty, starts with letter / `_` /
/// `$`, rest are letters / digits / `_` / `$`.
fn validate_field_name(s: &str) -> Result<(), &'static str> {
    let s = s.trim();
    if s.is_empty() {
        return Err("must not be empty");
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' && first != '$' {
        return Err("must start with a letter, `_` or `$`");
    }
    for ch in chars {
        if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$') {
            return Err("contains invalid character");
        }
    }
    Ok(())
}

/// Validate a JNI type signature suitable for a field slot. Accepts
/// the eight primitives, `L<path>;` object refs, and `[`-prefixed
/// arrays of either. Doesn't try to handle generics / wildcards —
/// those don't appear in a `.field` line.
fn validate_field_type(s: &str) -> Result<(), &'static str> {
    let s = s.trim();
    if s.is_empty() {
        return Err("must not be empty");
    }
    // Strip leading `[`s (array dims).
    let mut rest = s;
    let mut dims = 0;
    while rest.starts_with('[') {
        rest = &rest[1..];
        dims += 1;
        if dims > 255 {
            return Err("too many array dimensions");
        }
    }
    if rest.is_empty() {
        return Err("array element type missing");
    }
    if rest.len() == 1 {
        return match rest.chars().next().unwrap() {
            'Z' | 'B' | 'C' | 'S' | 'I' | 'J' | 'F' | 'D' => Ok(()),
            'V' => Err("`V` (void) isn't a valid field type"),
            _ => Err("not a primitive type letter"),
        };
    }
    // Object signature: L<path>;
    if !rest.starts_with('L') {
        return Err("expected primitive letter or `L<class>;`");
    }
    if !rest.ends_with(';') {
        return Err("missing trailing `;`");
    }
    let inner = &rest[1..rest.len() - 1];
    if inner.is_empty() {
        return Err("empty class name");
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

#[allow(clippy::too_many_arguments)]
pub fn render(
    state: &FieldEditState,
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

    let name_row = labelled_input(
        "Name",
        "field-name",
        &state.name,
        fg,
        dim,
        accent,
        state.focus == FieldFocus::Name,
        FieldFocus::Name,
        "fieldName",
        cx,
    );
    let type_row = labelled_input(
        "Type",
        "field-type",
        &state.signature,
        fg,
        dim,
        accent,
        state.focus == FieldFocus::Type,
        FieldFocus::Type,
        "Ljava/lang/String;",
        cx,
    );
    let initial_row = labelled_input(
        "Initial value (optional)",
        "field-initial",
        &state.initial_value,
        fg,
        dim,
        accent,
        state.focus == FieldFocus::InitialValue,
        FieldFocus::InitialValue,
        "",
        cx,
    );

    let card = div()
        .id("field-card")
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
        .child(header_row(fg, dim, state))
        .child(modifiers_row(state, fg, dim, accent, cx))
        .child(name_row)
        .child(type_row)
        .child(initial_row)
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
                this.cancel_field_edit(cx);
            }),
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}

fn header_row(fg: gpui::Rgba, dim: gpui::Rgba, state: &FieldEditState) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_sm()
                .text_color(fg)
                .child(SharedString::from("Field")),
        )
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(SharedString::from(format!(
                    "{}.{}",
                    state.class_name_display, state.original_name
                ))),
        )
}

fn modifiers_row(
    state: &FieldEditState,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let entity = cx.entity();
    let entity_for_vis = entity.clone();
    let on_visibility = move |vis: Visibility, cx: &mut App| {
        cx.update_entity(&entity_for_vis, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.field_edit.as_mut() {
                state.modifiers = set_visibility(&state.modifiers, vis);
                cx.notify();
            }
        });
    };
    let entity_for_toggle = entity;
    let on_toggle = move |m: Modifier, cx: &mut App| {
        cx.update_entity(&entity_for_toggle, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.field_edit.as_mut() {
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
            "field-mods",
            ModifierSite::Field,
            &state.modifiers,
            fg,
            dim,
            accent,
            on_visibility,
            on_toggle,
        ))
}

#[allow(clippy::too_many_arguments)]
fn labelled_input(
    label: &'static str,
    id: &'static str,
    input: &TextInput,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    focused: bool,
    focus_target: FieldFocus,
    placeholder: &'static str,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let entity = cx.entity();
    let on_click_target = focus_target;
    let on_pos_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
        cx.update_entity(&entity, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.field_edit.as_mut() {
                state.focus = on_click_target;
                if let Some(input) = state.focused_input_mut() {
                    input.set_cursor_pos(byte, shift);
                }
                cx.notify();
            }
        });
    };
    let inner =
        input.render_clickable(id, fg, dim, placeholder, "Courier New", on_pos_click);
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

fn annotations_section(
    state: &FieldEditState,
    summaries: &[(String, String)],
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let artifact = state.artifact.clone();
    let class_jni = state.class_jni.clone();
    let field_name = state.original_name.clone();
    let field_sig = state.original_signature_jni.clone();
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
                    let a = artifact.clone();
                    let c = class_jni.clone();
                    let n = field_name.clone();
                    let s = field_sig.clone();
                    div()
                        .id("field-add-annotation")
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
                                shell.open_field_annotation_editor(
                                    a.clone(),
                                    c.clone(),
                                    n.clone(),
                                    s.clone(),
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
            col = col.child(field_annotation_row(
                i, vis, jni, artifact.clone(), class_jni.clone(),
                field_name.clone(), field_sig.clone(), fg, dim, accent, cx,
            ));
        }
    }
    col
}

#[allow(clippy::too_many_arguments)]
fn field_annotation_row(
    index: usize,
    vis: &str,
    jni: &str,
    artifact: glass_db::ArtifactId,
    class_jni: String,
    field_name: String,
    field_sig: String,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    _accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let row_id: &'static str =
        Box::leak(format!("field-annotation-{index}").into_boxed_str());
    let edit_id: &'static str =
        Box::leak(format!("field-annotation-edit-{index}").into_boxed_str());
    let rm_id: &'static str =
        Box::leak(format!("field-annotation-rm-{index}").into_boxed_str());
    let summary = format!("{vis} {jni}");
    let ea = artifact.clone();
    let ec = class_jni.clone();
    let en = field_name.clone();
    let es = field_sig.clone();
    let ra = artifact;
    let rc = class_jni;
    let rn = field_name;
    let rs = field_sig;
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
                        shell.open_field_annotation_editor(
                            ea.clone(),
                            ec.clone(),
                            en.clone(),
                            es.clone(),
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
                        shell.remove_field_annotation(
                            ra.clone(),
                            rc.clone(),
                            rn.clone(),
                            rs.clone(),
                            index,
                            cx,
                        );
                    }),
                ),
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
        .id("field-save")
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
                    shell.commit_field_edit(cx);
                }),
            )
        });
    let cancel = div()
        .id("field-cancel")
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
                shell.cancel_field_edit(cx);
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

pub fn handle_named_key(shell: &mut Shell, key: &str, cx: &mut Context<Shell>) {
    if let Some(state) = shell.field_edit.as_mut() {
        if let Some(input) = state.focused_input_mut() {
            input.handle_key(key, false, false, false, None, cx);
            cx.notify();
        }
    }
}

pub fn handle_key(shell: &mut Shell, ks: &gpui::Keystroke, cx: &mut Context<Shell>) {
    let key = ks.key.as_str();
    if key == "escape" {
        shell.cancel_field_edit(cx);
        return;
    }
    if key == "enter" {
        shell.commit_field_edit(cx);
        return;
    }
    if key == "tab" {
        if let Some(state) = shell.field_edit.as_mut() {
            state.cycle_focus(ks.modifiers.shift);
            cx.notify();
        }
        return;
    }
    if let Some(state) = shell.field_edit.as_mut() {
        if let Some(input) = state.focused_input_mut() {
            input.handle_key(
                key,
                ks.modifiers.shift,
                ks.modifiers.platform || ks.modifiers.control,
                ks.modifiers.alt,
                ks.key_char.as_deref(),
                cx,
            );
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_names() {
        assert!(validate_field_name("count").is_ok());
        assert!(validate_field_name("_internal").is_ok());
        assert!(validate_field_name("$synthetic").is_ok());
        assert!(validate_field_name("a1B2_$").is_ok());
    }

    #[test]
    fn rejects_malformed_names() {
        assert!(validate_field_name("").is_err());
        assert!(validate_field_name(" ").is_err());
        assert!(validate_field_name("1foo").is_err());
        assert!(validate_field_name("foo bar").is_err());
        assert!(validate_field_name("foo-bar").is_err());
    }

    #[test]
    fn accepts_well_formed_types() {
        assert!(validate_field_type("I").is_ok());
        assert!(validate_field_type("Z").is_ok());
        assert!(validate_field_type("Ljava/lang/String;").is_ok());
        assert!(validate_field_type("[I").is_ok());
        assert!(validate_field_type("[[Ljava/lang/Object;").is_ok());
    }

    #[test]
    fn rejects_malformed_types() {
        assert!(validate_field_type("").is_err());
        assert!(validate_field_type("V").is_err()); // void not valid for a field
        assert!(validate_field_type("X").is_err());
        assert!(validate_field_type("L;").is_err());
        assert!(validate_field_type("Ljava/lang/String").is_err()); // missing ;
        assert!(validate_field_type("[").is_err());
    }
}
