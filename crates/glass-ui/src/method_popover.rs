//! Method header editor popover.
//!
//! Opens when the user double-clicks (or hits Enter on) a
//! `.method` line in a smali tab. Lets them edit the method's
//! name, JNI signature, modifiers (via
//! `modifier_picker::ModifierSite::Method`), constructor flag,
//! and the optional explicit `.registers` count. The method body
//! (per-op editor) is M1.2 — for now the popover preserves
//! `params`, `annotations`, `ops`, and `locals` from the original.
//!
//! Layout mirrors the class-decl and field popovers: a 720 px
//! card centred over the window with a dimming backdrop (click
//! cancels). Save stages the edit into the parent `SmaliClass`
//! in `bundle.smali_edits` — the registry is class-keyed, so a
//! method edit means cloning the class, swapping the one method
//! by `(original_name, original_signature_jni)`, and re-inserting.
//!
//! Method-level annotations are already covered by the recursive
//! annotation editor — once that's wired into this popover (same
//! way the field popover wires it) editing a method's annotations
//! becomes a child popover stacked above this one.

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};
use smali::types::{MethodSignature, Modifier, SmaliClass, SmaliMethod};

use crate::modifier_picker::{
    render_modifier_picker, set_visibility, toggle_modifier, ModifierSite, Visibility,
};
use crate::text_input::TextInput;
use crate::Shell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodFocus {
    Name,
    Signature,
    Registers,
}

/// Per-popover state. Lives on `Shell.method_edit` while open.
pub struct MethodEditState {
    pub artifact: glass_db::ArtifactId,
    pub class_jni: String,
    pub class_name_display: SharedString,
    /// Original `(name, signature_jni)` — used to locate the
    /// method inside the class on commit, since both are
    /// user-editable here.
    pub original_name: String,
    pub original_signature_jni: String,
    pub modifiers: Vec<Modifier>,
    pub constructor: bool,
    pub name: TextInput,
    pub signature: TextInput,
    /// Explicit `.registers` override. Empty means "use the
    /// existing `.locals` count" (smali's other way of declaring
    /// register usage). Non-empty must parse to a u32.
    pub registers: TextInput,
    pub focus: MethodFocus,
}

impl MethodEditState {
    pub fn from_method(
        artifact: glass_db::ArtifactId,
        class_jni: String,
        class: &SmaliClass,
        method: &SmaliMethod,
    ) -> Self {
        let registers_text = method
            .registers
            .map(|r| r.to_string())
            .unwrap_or_default();
        Self {
            artifact,
            class_jni,
            class_name_display: SharedString::from(class.name.as_java_type()),
            original_name: method.name.clone(),
            original_signature_jni: method.signature.to_jni(),
            modifiers: method.modifiers.clone(),
            constructor: method.constructor,
            name: TextInput::from_text(method.name.clone()),
            signature: TextInput::from_text(method.signature.to_jni()),
            registers: TextInput::from_text(registers_text),
            focus: MethodFocus::Name,
        }
    }

    pub fn focused_input_mut(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            MethodFocus::Name => Some(&mut self.name),
            MethodFocus::Signature => Some(&mut self.signature),
            MethodFocus::Registers => Some(&mut self.registers),
        }
    }

    pub fn cycle_focus(&mut self, reverse: bool) {
        let order = [MethodFocus::Name, MethodFocus::Signature, MethodFocus::Registers];
        let cur = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if reverse {
            (cur + order.len() - 1) % order.len()
        } else {
            (cur + 1) % order.len()
        };
        self.focus = order[next];
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_method_name(self.name.text())
            .map_err(|m| format!("Name: {m}"))?;
        validate_method_signature(self.signature.text())
            .map_err(|m| format!("Signature: {m}"))?;
        let r = self.registers.text().trim();
        if !r.is_empty() {
            r.parse::<u32>()
                .map_err(|_| "Registers: must be a non-negative integer".to_string())?;
        }
        // Java rule: a constructor's name is `<init>` (instance)
        // or `<clinit>` (class). Flagging this here keeps us from
        // staging a class whose DEX writer will reject the method
        // table.
        if self.constructor {
            let nm = self.name.text().trim();
            if nm != "<init>" && nm != "<clinit>" {
                return Err(
                    "Constructor must be named `<init>` or `<clinit>`".into(),
                );
            }
        }
        Ok(())
    }

    /// Splice the form values into `original_class`, replacing
    /// the method identified by `(original_name,
    /// original_signature_jni)`. Returns `None` if no matching
    /// method is found.
    pub fn build_modified(&self, original_class: &SmaliClass) -> Option<SmaliClass> {
        let mut out = original_class.clone();
        let idx = out.methods.iter().position(|m| {
            m.name == self.original_name
                && m.signature.to_jni() == self.original_signature_jni
        })?;
        let registers_value: Option<u32> = {
            let r = self.registers.text().trim();
            if r.is_empty() { None } else { r.parse().ok() }
        };
        let new_signature = MethodSignature::from_jni(self.signature.text().trim());
        let original = out.methods[idx].clone();
        out.methods[idx] = SmaliMethod {
            name: self.name.text().trim().to_string(),
            modifiers: self.modifiers.clone(),
            constructor: self.constructor,
            signature: new_signature,
            // Preserve untouched fields from the original.
            locals: original.locals,
            registers: registers_value,
            params: original.params,
            annotations: original.annotations,
            ops: original.ops,
        };
        Some(out)
    }
}

/// True if `line` is the first line of a method header — `.method`
/// plus its modifiers / name / signature. Method body lines and
/// `.end method` are intentionally not class-decl-popover targets.
pub fn line_is_method_decl(line: &str) -> bool {
    line.trim_start().starts_with(".method ")
}

fn validate_method_name(s: &str) -> Result<(), &'static str> {
    let s = s.trim();
    if s.is_empty() {
        return Err("must not be empty");
    }
    // Constructor names `<init>` / `<clinit>` are the two allowed
    // angle-bracket names in the JVM/Dalvik. Any other use of
    // `<` or `>` in a method name is invalid.
    if s == "<init>" || s == "<clinit>" {
        return Ok(());
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

/// Validate a JNI method signature: `(args)return`. Accepts the
/// same type vocabulary as `validate_field_type` plus `V` for
/// void in the return slot. Generic type parameters in `<…>` and
/// the `^Throws;` clause aren't validated here — the writer
/// passes them through verbatim.
fn validate_method_signature(s: &str) -> Result<(), &'static str> {
    let s = s.trim();
    let s = if s.starts_with('<') {
        // Skip type parameters like `<T:Ljava/lang/Object;>`.
        let close = s.find('>').ok_or("type-parameter `<` without `>`")?;
        &s[close + 1..]
    } else {
        s
    };
    let s = s
        .strip_prefix('(')
        .ok_or("must start with `(` (after any `<…>` type params)")?;
    let close = s.find(')').ok_or("missing `)`")?;
    let args = &s[..close];
    let ret = &s[close + 1..];
    let mut rest = args;
    while !rest.is_empty() {
        let (consumed, _err_offset) = parse_one_jni_type(rest, false)
            .map_err(|m| m)?;
        rest = &rest[consumed..];
    }
    // The return slot allows `V` plus any of the types `args`
    // does. `^Ljava/lang/Throwable;` clauses follow the return
    // and are optional — we accept either form.
    let (consumed, _) = parse_one_jni_type(ret, true)?;
    let tail = ret[consumed..].trim();
    if !tail.is_empty() && !tail.starts_with('^') {
        return Err("unexpected text after return type");
    }
    Ok(())
}

/// Parse one JNI type starting at `s`. Returns the byte length
/// consumed and `Ok(())`, or an error.
///
/// `allow_void` enables `V` (legal in a return slot, illegal in
/// args).
fn parse_one_jni_type(s: &str, allow_void: bool) -> Result<(usize, ()), &'static str> {
    let mut idx = 0;
    let bytes = s.as_bytes();
    // Array brackets.
    while bytes.get(idx) == Some(&b'[') {
        idx += 1;
        if idx > 255 {
            return Err("too many array dimensions");
        }
    }
    let first = *bytes.get(idx).ok_or("type slot is empty")?;
    match first {
        b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => Ok((idx + 1, ())),
        b'V' if allow_void => Ok((idx + 1, ())),
        b'V' => Err("`V` (void) is only valid as a return type"),
        b'L' => {
            // Find the closing `;`.
            let close = s[idx..]
                .find(';')
                .ok_or("object type missing trailing `;`")?;
            let inner = &s[idx + 1..idx + close];
            if inner.is_empty() {
                return Err("object type has empty class name");
            }
            for part in inner.split('/') {
                if part.is_empty() {
                    return Err("object type path component is empty");
                }
                let f = part.chars().next().unwrap();
                if !f.is_ascii_alphabetic() && f != '_' && f != '$' {
                    return Err(
                        "object type path component must start with a letter, `_` or `$`",
                    );
                }
                for ch in part.chars() {
                    if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$') {
                        return Err(
                            "object type path component has invalid character",
                        );
                    }
                }
            }
            Ok((idx + close + 1, ()))
        }
        b'T' => {
            // Type-variable signature: `T<identifier>;` — used in
            // generic method args, e.g. `(TT;)V`.
            let close = s[idx..]
                .find(';')
                .ok_or("type variable missing trailing `;`")?;
            let inner = &s[idx + 1..idx + close];
            if inner.is_empty() {
                return Err("type variable has empty name");
            }
            Ok((idx + close + 1, ()))
        }
        _ => Err("not a recognised JNI type letter"),
    }
}

// ----- Render -------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn render(
    state: &MethodEditState,
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
        "method-name",
        &state.name,
        fg,
        dim,
        accent,
        state.focus == MethodFocus::Name,
        MethodFocus::Name,
        "doSomething",
        cx,
    );
    let signature_row = labelled_input(
        "Signature (JNI)",
        "method-signature",
        &state.signature,
        fg,
        dim,
        accent,
        state.focus == MethodFocus::Signature,
        MethodFocus::Signature,
        "(Ljava/lang/String;)V",
        cx,
    );
    let registers_row = labelled_input(
        "Registers (optional override)",
        "method-registers",
        &state.registers,
        fg,
        dim,
        accent,
        state.focus == MethodFocus::Registers,
        MethodFocus::Registers,
        "",
        cx,
    );

    let card = div()
        .id("method-card")
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
        .child(constructor_row(state, fg, dim, accent, cx))
        .child(name_row)
        .child(signature_row)
        .child(registers_row)
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
                this.cancel_method_edit(cx);
            }),
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}

fn header_row(fg: gpui::Rgba, dim: gpui::Rgba, state: &MethodEditState) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_sm()
                .text_color(fg)
                .child(SharedString::from("Method")),
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
    state: &MethodEditState,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let entity = cx.entity();
    let entity_for_vis = entity.clone();
    let on_visibility = move |vis: Visibility, cx: &mut App| {
        cx.update_entity(&entity_for_vis, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.method_edit.as_mut() {
                state.modifiers = set_visibility(&state.modifiers, vis);
                cx.notify();
            }
        });
    };
    let entity_for_toggle = entity;
    let on_toggle = move |m: Modifier, cx: &mut App| {
        cx.update_entity(&entity_for_toggle, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.method_edit.as_mut() {
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
            "method-mods",
            ModifierSite::Method,
            &state.modifiers,
            fg,
            dim,
            accent,
            on_visibility,
            on_toggle,
        ))
}

fn constructor_row(
    state: &MethodEditState,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let checked = state.constructor;
    let box_ = div()
        .id("method-constructor")
        .w(px(14.))
        .h(px(14.))
        .border_1()
        .border_color(if checked { accent } else { dim })
        .rounded_sm()
        .bg(if checked { accent } else { gpui::rgba(0x00000000) })
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                if let Some(state) = shell.method_edit.as_mut() {
                    state.constructor = !state.constructor;
                    cx.notify();
                }
            }),
        );
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(box_)
        .child(
            div()
                .text_xs()
                .text_color(fg)
                .child(SharedString::from("Constructor")),
        )
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(
                    "(name must be `<init>` or `<clinit>`)",
                )),
        )
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
    focus_target: MethodFocus,
    placeholder: &'static str,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let entity = cx.entity();
    let on_click_target = focus_target;
    let on_pos_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
        cx.update_entity(&entity, |shell, cx: &mut Context<Shell>| {
            if let Some(state) = shell.method_edit.as_mut() {
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
    state: &MethodEditState,
    summaries: &[(String, String)],
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let artifact = state.artifact.clone();
    let class_jni = state.class_jni.clone();
    let method_name = state.original_name.clone();
    let method_sig = state.original_signature_jni.clone();
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
                    let n = method_name.clone();
                    let s = method_sig.clone();
                    div()
                        .id("method-add-annotation")
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
                                shell.open_method_annotation_editor(
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
            col = col.child(method_annotation_row(
                i, vis, jni, artifact.clone(), class_jni.clone(),
                method_name.clone(), method_sig.clone(), fg, dim, accent, cx,
            ));
        }
    }
    col
}

#[allow(clippy::too_many_arguments)]
fn method_annotation_row(
    index: usize,
    vis: &str,
    jni: &str,
    artifact: glass_db::ArtifactId,
    class_jni: String,
    method_name: String,
    method_sig: String,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    _accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let row_id: &'static str =
        Box::leak(format!("method-annotation-{index}").into_boxed_str());
    let edit_id: &'static str =
        Box::leak(format!("method-annotation-edit-{index}").into_boxed_str());
    let rm_id: &'static str =
        Box::leak(format!("method-annotation-rm-{index}").into_boxed_str());
    let summary = format!("{vis} {jni}");
    let ea = artifact.clone();
    let ec = class_jni.clone();
    let en = method_name.clone();
    let es = method_sig.clone();
    let ra = artifact;
    let rc = class_jni;
    let rn = method_name;
    let rs = method_sig;
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
                        shell.open_method_annotation_editor(
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
                        shell.remove_method_annotation(
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
        .id("method-save")
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
                    shell.commit_method_edit(cx);
                }),
            )
        });
    let cancel = div()
        .id("method-cancel")
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
                shell.cancel_method_edit(cx);
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
    if let Some(state) = shell.method_edit.as_mut() {
        if let Some(input) = state.focused_input_mut() {
            input.handle_key(key, false, false, false, None, cx);
            cx.notify();
        }
    }
}

pub fn handle_key(shell: &mut Shell, ks: &gpui::Keystroke, cx: &mut Context<Shell>) {
    let key = ks.key.as_str();
    if key == "escape" {
        shell.cancel_method_edit(cx);
        return;
    }
    if key == "enter" {
        shell.commit_method_edit(cx);
        return;
    }
    if key == "tab" {
        if let Some(state) = shell.method_edit.as_mut() {
            state.cycle_focus(ks.modifiers.shift);
            cx.notify();
        }
        return;
    }
    if let Some(state) = shell.method_edit.as_mut() {
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
        assert!(validate_method_name("foo").is_ok());
        assert!(validate_method_name("doX").is_ok());
        assert!(validate_method_name("<init>").is_ok());
        assert!(validate_method_name("<clinit>").is_ok());
        assert!(validate_method_name("$synthetic").is_ok());
    }

    #[test]
    fn rejects_bad_names() {
        assert!(validate_method_name("").is_err());
        assert!(validate_method_name("1foo").is_err());
        assert!(validate_method_name("foo bar").is_err());
        assert!(validate_method_name("<custom>").is_err());
    }

    #[test]
    fn accepts_well_formed_signatures() {
        assert!(validate_method_signature("()V").is_ok());
        assert!(validate_method_signature("(I)V").is_ok());
        assert!(validate_method_signature("(Ljava/lang/String;I)Z").is_ok());
        assert!(validate_method_signature("([B[I)Ljava/lang/Object;").is_ok());
        // Type parameters.
        assert!(validate_method_signature("<T:Ljava/lang/Object;>(TT;)V").is_ok());
    }

    #[test]
    fn rejects_bad_signatures() {
        assert!(validate_method_signature("").is_err());
        assert!(validate_method_signature("V").is_err()); // missing parens
        assert!(validate_method_signature("(V)V").is_err()); // void as arg
        assert!(validate_method_signature("(I)").is_err()); // missing return
        assert!(validate_method_signature("(Ljava/lang/Foo)V").is_err()); // missing ;
    }
}
