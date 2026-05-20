//! Recursive annotation editor.
//!
//! Opens as a stacked popover above the class-decl or field
//! popover — the parent stays mounted underneath so saving an
//! annotation returns the user to the same form they came from.
//!
//! SubAnnotation values open a second stacked annotation popover
//! over the first; Esc on a child returns to its parent without
//! touching the bundle. The stack model is a `Vec<AnnotationFrame>`
//! on `Shell.annotation_stack` — the top frame receives keystrokes
//! and renders; everything below is dimmed behind it.
//!
//! Where edits land:
//!   * Class-level annotations save into
//!     `SmaliClass.annotations[index]` (or push if `index` was
//!     `None` — i.e. the user picked Add).
//!   * Field-level annotations save into
//!     `SmaliClass.fields[<by name+sig>].annotations[index]`.
//!   * SubAnnotation frames save into the parent frame's
//!     `elements[parent_element_index].value` as a fresh
//!     `AnnotationValueDraft::SubAnnotation`.
//!
//! Validation is minimal: annotation type must look like a JNI
//! object signature, each element name must be a Java identifier,
//! SubAnnotation values delegate to recursion. Single / Array /
//! Enum string contents are treated as opaque smali tokens — we
//! don't try to parse them since the smali crate accepts a wide
//! range of literal shapes.

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};
use smali::types::{
    AnnotationElement, AnnotationValue, AnnotationVisibility, ObjectIdentifier,
    SmaliAnnotation, TypeSignature,
};

use crate::text_input::TextInput;
use crate::Shell;

/// Stack of open annotation frames. Lives on `Shell.annotation_stack`.
/// Empty when no annotation popover is open. Pushing a SubAnnotation
/// editor appends; saving a child copies its frame into the parent
/// and pops; saving the root writes through `root_target`.
pub struct AnnotationStack {
    pub root_target: AnnotationTarget,
    pub frames: Vec<AnnotationFrame>,
}

/// Where the bottom frame writes back at root-save time. The
/// concrete class look-up happens on commit so we don't hold any
/// references across the editor's lifetime.
#[derive(Clone)]
pub enum AnnotationTarget {
    ClassAnnotation {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        /// `Some(i)` when editing an existing annotation, `None`
        /// when adding a new one (Save will push).
        index: Option<usize>,
    },
    FieldAnnotation {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        /// Identifies the field by its original `(name, JNI sig)`.
        /// We use the original (not the in-progress edits the
        /// user might be making in the field popover) — the field
        /// popover commits its own changes separately, so
        /// annotation editing always targets the field as it
        /// currently exists in the staged or original class.
        field_name: String,
        field_signature_jni: String,
        index: Option<usize>,
    },
    MethodAnnotation {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        index: Option<usize>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationFocus {
    Type,
    ElementName(usize),
    ElementValueSingle(usize),
    ElementValueArray(usize, usize),
    ElementValueEnumClass(usize),
    ElementValueEnumName(usize),
}

pub struct AnnotationFrame {
    pub visibility: AnnotationVisibility,
    pub annotation_type: TextInput,
    pub elements: Vec<AnnotationElementDraft>,
    pub focus: AnnotationFocus,
    /// `Some(i)` on a SubAnnotation frame — saves into the
    /// parent frame's `elements[i].value`. `None` on the root
    /// frame — saves writes through `AnnotationStack.root_target`.
    pub parent_element_index: Option<usize>,
}

pub struct AnnotationElementDraft {
    pub name: TextInput,
    pub value: AnnotationValueDraft,
}

pub enum AnnotationValueDraft {
    Single(TextInput),
    Array(Vec<TextInput>),
    /// Snapshot of a SubAnnotation. Editing pushes a new frame
    /// seeded from this; saving the child overwrites it.
    SubAnnotation(Box<SmaliAnnotation>),
    Enum {
        class_jni: TextInput,
        name: TextInput,
    },
}

impl AnnotationValueDraft {
    /// Tag for the kind-selector pills.
    pub fn kind(&self) -> ValueKind {
        match self {
            Self::Single(_) => ValueKind::Single,
            Self::Array(_) => ValueKind::Array,
            Self::SubAnnotation(_) => ValueKind::SubAnnotation,
            Self::Enum { .. } => ValueKind::Enum,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Single,
    Array,
    SubAnnotation,
    Enum,
}

impl ValueKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Single => "Single",
            Self::Array => "Array",
            Self::SubAnnotation => "SubAnnotation",
            Self::Enum => "Enum",
        }
    }
}

// ----- Conversions ---------------------------------------------------

impl AnnotationFrame {
    pub fn from_annotation(
        ann: &SmaliAnnotation,
        parent_element_index: Option<usize>,
    ) -> Self {
        let elements = ann.elements.iter().map(element_to_draft).collect();
        Self {
            visibility: ann.visibility.clone(),
            annotation_type: TextInput::from_text(ann.annotation_type.to_jni()),
            elements,
            focus: AnnotationFocus::Type,
            parent_element_index,
        }
    }

    /// Empty starter — used when adding a brand-new annotation.
    pub fn blank(parent_element_index: Option<usize>) -> Self {
        Self {
            visibility: AnnotationVisibility::Runtime,
            annotation_type: TextInput::new(),
            elements: Vec::new(),
            focus: AnnotationFocus::Type,
            parent_element_index,
        }
    }

    pub fn to_annotation(&self) -> SmaliAnnotation {
        SmaliAnnotation {
            visibility: self.visibility.clone(),
            annotation_type: TypeSignature::from_jni(self.annotation_type.text().trim()),
            elements: self.elements.iter().map(draft_to_element).collect(),
        }
    }

    pub fn focused_input_mut(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            AnnotationFocus::Type => Some(&mut self.annotation_type),
            AnnotationFocus::ElementName(i) => {
                self.elements.get_mut(i).map(|e| &mut e.name)
            }
            AnnotationFocus::ElementValueSingle(i) => {
                self.elements.get_mut(i).and_then(|e| match &mut e.value {
                    AnnotationValueDraft::Single(t) => Some(t),
                    _ => None,
                })
            }
            AnnotationFocus::ElementValueArray(i, j) => {
                self.elements.get_mut(i).and_then(|e| match &mut e.value {
                    AnnotationValueDraft::Array(v) => v.get_mut(j),
                    _ => None,
                })
            }
            AnnotationFocus::ElementValueEnumClass(i) => {
                self.elements.get_mut(i).and_then(|e| match &mut e.value {
                    AnnotationValueDraft::Enum { class_jni, .. } => Some(class_jni),
                    _ => None,
                })
            }
            AnnotationFocus::ElementValueEnumName(i) => {
                self.elements.get_mut(i).and_then(|e| match &mut e.value {
                    AnnotationValueDraft::Enum { name, .. } => Some(name),
                    _ => None,
                })
            }
        }
    }

    pub fn cycle_focus(&mut self, reverse: bool) {
        let order = self.focus_order();
        if order.is_empty() {
            return;
        }
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

    fn focus_order(&self) -> Vec<AnnotationFocus> {
        let mut out = vec![AnnotationFocus::Type];
        for (i, e) in self.elements.iter().enumerate() {
            out.push(AnnotationFocus::ElementName(i));
            match &e.value {
                AnnotationValueDraft::Single(_) => {
                    out.push(AnnotationFocus::ElementValueSingle(i))
                }
                AnnotationValueDraft::Array(v) => {
                    for j in 0..v.len() {
                        out.push(AnnotationFocus::ElementValueArray(i, j));
                    }
                }
                AnnotationValueDraft::Enum { .. } => {
                    out.push(AnnotationFocus::ElementValueEnumClass(i));
                    out.push(AnnotationFocus::ElementValueEnumName(i));
                }
                // SubAnnotation isn't focusable inline — user clicks
                // "Edit…" to push a new frame.
                AnnotationValueDraft::SubAnnotation(_) => {}
            }
        }
        out
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_jni_object_sig(self.annotation_type.text())
            .map_err(|m| format!("Type: {m}"))?;
        for (i, e) in self.elements.iter().enumerate() {
            validate_java_ident(e.name.text())
                .map_err(|m| format!("Element {} name: {m}", i + 1))?;
            match &e.value {
                AnnotationValueDraft::Single(t) => {
                    if t.text().trim().is_empty() {
                        return Err(format!("Element {} value is empty", i + 1));
                    }
                }
                AnnotationValueDraft::Array(v) => {
                    for (j, item) in v.iter().enumerate() {
                        if item.text().trim().is_empty() {
                            return Err(format!(
                                "Element {} array entry {} is empty",
                                i + 1,
                                j + 1
                            ));
                        }
                    }
                }
                AnnotationValueDraft::Enum { class_jni, name } => {
                    validate_jni_object_sig(class_jni.text()).map_err(|m| {
                        format!("Element {} enum class: {m}", i + 1)
                    })?;
                    if name.text().trim().is_empty() {
                        return Err(format!(
                            "Element {} enum name is empty",
                            i + 1
                        ));
                    }
                }
                AnnotationValueDraft::SubAnnotation(_) => {
                    // Sub-annotations are validated when the user
                    // opens / saves them via the nested popover —
                    // the snapshot we hold is already a valid
                    // SmaliAnnotation from the previous save.
                }
            }
        }
        Ok(())
    }
}

fn element_to_draft(e: &AnnotationElement) -> AnnotationElementDraft {
    let value = match &e.value {
        AnnotationValue::Single(s) => {
            AnnotationValueDraft::Single(TextInput::from_text(s.clone()))
        }
        AnnotationValue::Array(v) => AnnotationValueDraft::Array(
            v.iter().map(|s| TextInput::from_text(s.clone())).collect(),
        ),
        AnnotationValue::SubAnnotation(s) => {
            AnnotationValueDraft::SubAnnotation(Box::new(s.clone()))
        }
        AnnotationValue::Enum(obj, name) => AnnotationValueDraft::Enum {
            class_jni: TextInput::from_text(obj.as_jni_type()),
            name: TextInput::from_text(name.clone()),
        },
    };
    AnnotationElementDraft {
        name: TextInput::from_text(e.name.clone()),
        value,
    }
}

fn draft_to_element(d: &AnnotationElementDraft) -> AnnotationElement {
    let value = match &d.value {
        AnnotationValueDraft::Single(t) => {
            AnnotationValue::Single(t.text().trim().to_string())
        }
        AnnotationValueDraft::Array(v) => AnnotationValue::Array(
            v.iter().map(|t| t.text().trim().to_string()).collect(),
        ),
        AnnotationValueDraft::SubAnnotation(s) => {
            AnnotationValue::SubAnnotation((**s).clone())
        }
        AnnotationValueDraft::Enum { class_jni, name } => AnnotationValue::Enum(
            ObjectIdentifier::from_jni_type(class_jni.text().trim()),
            name.text().trim().to_string(),
        ),
    };
    AnnotationElement {
        name: d.name.text().trim().to_string(),
        value,
    }
}

// ----- Validators ---------------------------------------------------

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

fn validate_java_ident(s: &str) -> Result<(), &'static str> {
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

// ----- Render -------------------------------------------------------

pub fn render(
    stack: &AnnotationStack,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let depth = stack.frames.len().saturating_sub(1);
    let Some(frame) = stack.frames.last() else {
        return div().into_any_element();
    };
    let validation = frame.validate();
    let save_enabled = validation.is_ok();

    let header = header_row(fg, dim, depth, frame);
    let visibility_row = visibility_row(frame, fg, dim, accent, cx);
    let type_row = type_input_row(frame, fg, dim, accent, cx);
    let elements_section = elements_section(frame, fg, dim, accent, cx);
    let actions_row = bottom_actions(frame, fg, dim, border, cx);
    let validation_msg = validation_row(validation.err(), dim);
    let footer = footer_row(save_enabled, depth > 0, fg, dim, border, accent, cx);

    let card = div()
        .id("annotation-card")
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
        .overflow_y_scroll()
        .occlude()
        .child(header)
        .child(visibility_row)
        .child(type_row)
        .child(elements_section)
        .child(actions_row)
        .child(validation_msg)
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
                this.cancel_annotation_frame(cx);
            }),
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}

fn header_row(
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    depth: usize,
    frame: &AnnotationFrame,
) -> gpui::Div {
    let title = if depth == 0 {
        "Annotation".to_string()
    } else {
        format!("SubAnnotation (depth {depth})")
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div().text_sm().text_color(fg).child(SharedString::from(title)),
        )
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(SharedString::from(if frame.annotation_type.text().is_empty() {
                    "(type not set)".to_string()
                } else {
                    frame.annotation_type.text().to_string()
                })),
        )
}

fn visibility_row(
    frame: &AnnotationFrame,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let entity = cx.entity();
    let mk_pill = move |idx: usize,
                        target: AnnotationVisibility,
                        label: &'static str,
                        active: bool|
          -> gpui::Stateful<gpui::Div> {
        let id: &'static str = Box::leak(format!("ann-vis-{idx}").into_boxed_str());
        let entity = entity.clone();
        div()
            .id(id)
            .px_2()
            .py_0p5()
            .text_xs()
            .border_1()
            .border_color(if active { accent } else { dim })
            .text_color(if active { accent } else { fg })
            .rounded_sm()
            .cursor_pointer()
            .child(SharedString::from(label))
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |_ev, _w, cx: &mut App| {
                    let target = target.clone();
                    cx.update_entity(&entity, move |shell, cx: &mut Context<Shell>| {
                        if let Some(stack) = shell.annotation_stack.as_mut() {
                            if let Some(frame) = stack.frames.last_mut() {
                                frame.visibility = target;
                                cx.notify();
                            }
                        }
                    });
                },
            )
    };
    let cur = match frame.visibility {
        AnnotationVisibility::Build => 0,
        AnnotationVisibility::Runtime => 1,
        AnnotationVisibility::System => 2,
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(dim).child(SharedString::from("Visibility")))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(mk_pill(0, AnnotationVisibility::Build, "build", cur == 0))
                .child(mk_pill(1, AnnotationVisibility::Runtime, "runtime", cur == 1))
                .child(mk_pill(2, AnnotationVisibility::System, "system", cur == 2)),
        )
}

fn type_input_row(
    frame: &AnnotationFrame,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let entity = cx.entity();
    let on_pos_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
        cx.update_entity(&entity, |shell, cx: &mut Context<Shell>| {
            if let Some(stack) = shell.annotation_stack.as_mut() {
                if let Some(frame) = stack.frames.last_mut() {
                    frame.focus = AnnotationFocus::Type;
                    if let Some(input) = frame.focused_input_mut() {
                        input.set_cursor_pos(byte, shift);
                    }
                    cx.notify();
                }
            }
        });
    };
    let focused = frame.focus == AnnotationFocus::Type;
    let inner = frame.annotation_type.render_clickable(
        "ann-type",
        fg,
        dim,
        "Lcom/example/Annot;",
        "Courier New",
        on_pos_click,
    );
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(dim).child(SharedString::from("Type (JNI)")))
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

fn elements_section(
    frame: &AnnotationFrame,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let mut col = div()
        .flex()
        .flex_col()
        .gap_2()
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
                        .child(SharedString::from("Elements")),
                )
                .child(
                    div()
                        .id("ann-add-element")
                        .px_2()
                        .py_0p5()
                        .text_xs()
                        .text_color(fg)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(SharedString::from("+ add element"))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|shell, _ev, _w, cx| {
                                if let Some(stack) = shell.annotation_stack.as_mut() {
                                    if let Some(frame) = stack.frames.last_mut() {
                                        frame.elements.push(AnnotationElementDraft {
                                            name: TextInput::new(),
                                            value: AnnotationValueDraft::Single(
                                                TextInput::new(),
                                            ),
                                        });
                                        let new_idx = frame.elements.len() - 1;
                                        frame.focus = AnnotationFocus::ElementName(new_idx);
                                        cx.notify();
                                    }
                                }
                            }),
                        ),
                ),
        );

    if frame.elements.is_empty() {
        col = col.child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(SharedString::from("(no elements)")),
        );
    } else {
        for (i, e) in frame.elements.iter().enumerate() {
            col = col.child(element_row(i, e, frame.focus, fg, dim, accent, cx));
        }
    }
    col
}

#[allow(clippy::too_many_arguments)]
fn element_row(
    index: usize,
    elem: &AnnotationElementDraft,
    focus: AnnotationFocus,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let row_id: &'static str = Box::leak(format!("ann-elem-{index}").into_boxed_str());
    let name_focused = focus == AnnotationFocus::ElementName(index);

    let entity_for_name = cx.entity();
    let on_name_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
        cx.update_entity(&entity_for_name, |shell, cx: &mut Context<Shell>| {
            if let Some(stack) = shell.annotation_stack.as_mut() {
                if let Some(frame) = stack.frames.last_mut() {
                    frame.focus = AnnotationFocus::ElementName(index);
                    if let Some(input) = frame.focused_input_mut() {
                        input.set_cursor_pos(byte, shift);
                    }
                    cx.notify();
                }
            }
        });
    };
    let name_id: &'static str =
        Box::leak(format!("ann-elem-name-{index}").into_boxed_str());
    let name_input = elem
        .name
        .render_clickable(name_id, fg, dim, "name", "Courier New", on_name_click);
    let name_box = div()
        .w(px(180.))
        .border_1()
        .border_color(if name_focused { accent } else { gpui::rgba(0x00000000) })
        .rounded_sm()
        .child(name_input);

    let kind_pills = value_kind_pills(index, elem.value.kind(), fg, dim, accent, cx);

    let value_block = match &elem.value {
        AnnotationValueDraft::Single(t) => render_value_single(index, t, focus, fg, dim, accent, cx).into_any_element(),
        AnnotationValueDraft::Array(items) => {
            render_value_array(index, items, focus, fg, dim, accent, cx).into_any_element()
        }
        AnnotationValueDraft::Enum { class_jni, name } => {
            render_value_enum(index, class_jni, name, focus, fg, dim, accent, cx)
                .into_any_element()
        }
        AnnotationValueDraft::SubAnnotation(s) => {
            render_value_subannotation(index, s, fg, dim, accent, cx).into_any_element()
        }
    };

    let remove = div()
        .id(Box::leak(format!("ann-elem-rm-{index}").into_boxed_str()) as &'static str)
        .text_xs()
        .text_color(dim)
        .cursor_pointer()
        .hover(|s| s.text_color(crate::theme::current().errors.severe.rgba()))
        .child(SharedString::from("× remove element"))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |shell, _ev, _w, cx| {
                if let Some(stack) = shell.annotation_stack.as_mut() {
                    if let Some(frame) = stack.frames.last_mut() {
                        if index < frame.elements.len() {
                            frame.elements.remove(index);
                            frame.focus = AnnotationFocus::Type;
                            cx.notify();
                        }
                    }
                }
            }),
        );

    div()
        .id(row_id)
        .flex()
        .flex_col()
        .gap_1()
        .pl_2()
        .border_l_2()
        .border_color(dim)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(name_box)
                .child(div().text_xs().text_color(dim).child(SharedString::from("="))),
        )
        .child(kind_pills)
        .child(value_block)
        .child(remove)
}

fn value_kind_pills(
    elem_index: usize,
    current: ValueKind,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let mk = move |kind: ValueKind, label: &'static str| -> gpui::Stateful<gpui::Div> {
        let id: &'static str = Box::leak(
            format!("ann-kind-{}-{}", elem_index, label).into_boxed_str(),
        );
        let active = kind == current;
        div()
            .id(id)
            .px_2()
            .py_0p5()
            .text_xs()
            .border_1()
            .border_color(if active { accent } else { dim })
            .text_color(if active { accent } else { fg })
            .rounded_sm()
            .cursor_pointer()
            .child(SharedString::from(label))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |shell, _ev, _w, cx| {
                    if let Some(stack) = shell.annotation_stack.as_mut() {
                        if let Some(frame) = stack.frames.last_mut() {
                            if let Some(e) = frame.elements.get_mut(elem_index) {
                                e.value = match kind {
                                    ValueKind::Single => AnnotationValueDraft::Single(
                                        TextInput::new(),
                                    ),
                                    ValueKind::Array => AnnotationValueDraft::Array(vec![
                                        TextInput::new(),
                                    ]),
                                    ValueKind::SubAnnotation => {
                                        AnnotationValueDraft::SubAnnotation(Box::new(
                                            SmaliAnnotation {
                                                visibility:
                                                    AnnotationVisibility::Runtime,
                                                annotation_type: TypeSignature::from_jni(
                                                    "Ljava/lang/Object;",
                                                ),
                                                elements: Vec::new(),
                                            },
                                        ))
                                    }
                                    ValueKind::Enum => AnnotationValueDraft::Enum {
                                        class_jni: TextInput::from_text(
                                            "Lcom/example/Enum;".to_string(),
                                        ),
                                        name: TextInput::new(),
                                    },
                                };
                                frame.focus = AnnotationFocus::ElementName(elem_index);
                                cx.notify();
                            }
                        }
                    }
                }),
            )
    };
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(mk(ValueKind::Single, ValueKind::Single.label()))
        .child(mk(ValueKind::Array, ValueKind::Array.label()))
        .child(mk(ValueKind::SubAnnotation, ValueKind::SubAnnotation.label()))
        .child(mk(ValueKind::Enum, ValueKind::Enum.label()))
}

#[allow(clippy::too_many_arguments)]
fn render_value_single(
    elem_index: usize,
    input: &TextInput,
    focus: AnnotationFocus,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let id: &'static str =
        Box::leak(format!("ann-val-single-{elem_index}").into_boxed_str());
    let focused = focus == AnnotationFocus::ElementValueSingle(elem_index);
    let entity = cx.entity();
    let on_pos_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
        cx.update_entity(&entity, |shell, cx: &mut Context<Shell>| {
            if let Some(stack) = shell.annotation_stack.as_mut() {
                if let Some(frame) = stack.frames.last_mut() {
                    frame.focus = AnnotationFocus::ElementValueSingle(elem_index);
                    if let Some(input) = frame.focused_input_mut() {
                        input.set_cursor_pos(byte, shift);
                    }
                    cx.notify();
                }
            }
        });
    };
    let inner = input.render_clickable(id, fg, dim, "\"value\"", "Courier New", on_pos_click);
    div()
        .w_full()
        .min_w(px(0.))
        .overflow_x_hidden()
        .border_1()
        .border_color(if focused { accent } else { gpui::rgba(0x00000000) })
        .rounded_sm()
        .child(inner)
}

#[allow(clippy::too_many_arguments)]
fn render_value_array(
    elem_index: usize,
    items: &[TextInput],
    focus: AnnotationFocus,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let mut col = div().flex().flex_col().gap_1();
    for (j, item) in items.iter().enumerate() {
        let id: &'static str =
            Box::leak(format!("ann-val-arr-{elem_index}-{j}").into_boxed_str());
        let focused = focus == AnnotationFocus::ElementValueArray(elem_index, j);
        let entity = cx.entity();
        let on_pos_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
            cx.update_entity(&entity, |shell, cx: &mut Context<Shell>| {
                if let Some(stack) = shell.annotation_stack.as_mut() {
                    if let Some(frame) = stack.frames.last_mut() {
                        frame.focus = AnnotationFocus::ElementValueArray(elem_index, j);
                        if let Some(input) = frame.focused_input_mut() {
                            input.set_cursor_pos(byte, shift);
                        }
                        cx.notify();
                    }
                }
            });
        };
        let inner =
            item.render_clickable(id, fg, dim, "entry", "Courier New", on_pos_click);
        let rm_id: &'static str =
            Box::leak(format!("ann-val-arr-rm-{elem_index}-{j}").into_boxed_str());
        let row = div()
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
                    .border_color(if focused { accent } else { gpui::rgba(0x00000000) })
                    .rounded_sm()
                    .child(inner),
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
                            if let Some(stack) = shell.annotation_stack.as_mut() {
                                if let Some(frame) = stack.frames.last_mut() {
                                    if let Some(e) =
                                        frame.elements.get_mut(elem_index)
                                    {
                                        if let AnnotationValueDraft::Array(v) =
                                            &mut e.value
                                        {
                                            if j < v.len() {
                                                v.remove(j);
                                                frame.focus =
                                                    AnnotationFocus::ElementName(elem_index);
                                                cx.notify();
                                            }
                                        }
                                    }
                                }
                            }
                        }),
                    ),
            );
        col = col.child(row);
    }
    let add_id: &'static str =
        Box::leak(format!("ann-val-arr-add-{elem_index}").into_boxed_str());
    col.child(
        div()
            .id(add_id)
            .px_2()
            .py_0p5()
            .text_xs()
            .text_color(fg)
            .cursor_pointer()
            .hover(|s| s.underline())
            .child(SharedString::from("+ add entry"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |shell, _ev, _w, cx| {
                    if let Some(stack) = shell.annotation_stack.as_mut() {
                        if let Some(frame) = stack.frames.last_mut() {
                            if let Some(e) = frame.elements.get_mut(elem_index) {
                                if let AnnotationValueDraft::Array(v) = &mut e.value {
                                    v.push(TextInput::new());
                                    let j = v.len() - 1;
                                    frame.focus =
                                        AnnotationFocus::ElementValueArray(elem_index, j);
                                    cx.notify();
                                }
                            }
                        }
                    }
                }),
            ),
    )
}

#[allow(clippy::too_many_arguments)]
fn render_value_enum(
    elem_index: usize,
    class_jni: &TextInput,
    name: &TextInput,
    focus: AnnotationFocus,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let class_id: &'static str =
        Box::leak(format!("ann-val-enum-class-{elem_index}").into_boxed_str());
    let name_id: &'static str =
        Box::leak(format!("ann-val-enum-name-{elem_index}").into_boxed_str());
    let class_focused = focus == AnnotationFocus::ElementValueEnumClass(elem_index);
    let name_focused = focus == AnnotationFocus::ElementValueEnumName(elem_index);
    let entity_for_class = cx.entity();
    let on_class_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
        cx.update_entity(&entity_for_class, |shell, cx: &mut Context<Shell>| {
            if let Some(stack) = shell.annotation_stack.as_mut() {
                if let Some(frame) = stack.frames.last_mut() {
                    frame.focus = AnnotationFocus::ElementValueEnumClass(elem_index);
                    if let Some(input) = frame.focused_input_mut() {
                        input.set_cursor_pos(byte, shift);
                    }
                    cx.notify();
                }
            }
        });
    };
    let entity_for_name = cx.entity();
    let on_name_click = move |byte: usize, shift: bool, cx: &mut gpui::App| {
        cx.update_entity(&entity_for_name, |shell, cx: &mut Context<Shell>| {
            if let Some(stack) = shell.annotation_stack.as_mut() {
                if let Some(frame) = stack.frames.last_mut() {
                    frame.focus = AnnotationFocus::ElementValueEnumName(elem_index);
                    if let Some(input) = frame.focused_input_mut() {
                        input.set_cursor_pos(byte, shift);
                    }
                    cx.notify();
                }
            }
        });
    };
    let class_inner =
        class_jni.render_clickable(class_id, fg, dim, "Lcom/Enum;", "Courier New", on_class_click);
    let name_inner =
        name.render_clickable(name_id, fg, dim, "NAME", "Courier New", on_name_click);
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_x_hidden()
                .border_1()
                .border_color(if class_focused { accent } else { gpui::rgba(0x00000000) })
                .rounded_sm()
                .child(class_inner),
        )
        .child(div().text_xs().text_color(dim).child(SharedString::from("->")))
        .child(
            div()
                .w(px(160.))
                .min_w(px(0.))
                .overflow_x_hidden()
                .border_1()
                .border_color(if name_focused { accent } else { gpui::rgba(0x00000000) })
                .rounded_sm()
                .child(name_inner),
        )
}

fn render_value_subannotation(
    elem_index: usize,
    snapshot: &SmaliAnnotation,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    _accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let edit_id: &'static str =
        Box::leak(format!("ann-val-sub-edit-{elem_index}").into_boxed_str());
    let summary = format!(
        "{} {}  ({} element{})",
        snapshot.visibility.to_str(),
        snapshot.annotation_type.to_jni(),
        snapshot.elements.len(),
        if snapshot.elements.len() == 1 { "" } else { "s" }
    );
    div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .text_xs()
                .text_color(dim)
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
                        shell.push_sub_annotation_frame(elem_index, cx);
                    }),
                ),
        )
}

fn bottom_actions(
    _frame: &AnnotationFrame,
    fg: gpui::Rgba,
    _dim: gpui::Rgba,
    border: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let add = div()
        .id("ann-add-element-bottom")
        .px_3()
        .py_1p5()
        .border_1()
        .border_color(border)
        .rounded_sm()
        .text_xs()
        .text_color(fg)
        .cursor_pointer()
        .child(SharedString::from("+ add element"))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                if let Some(stack) = shell.annotation_stack.as_mut() {
                    if let Some(frame) = stack.frames.last_mut() {
                        frame.elements.push(AnnotationElementDraft {
                            name: TextInput::new(),
                            value: AnnotationValueDraft::Single(TextInput::new()),
                        });
                        let new_idx = frame.elements.len() - 1;
                        frame.focus = AnnotationFocus::ElementName(new_idx);
                        cx.notify();
                    }
                }
            }),
        );
    div().flex().flex_row().gap_2().justify_start().child(add)
}

fn validation_row(err: Option<String>, dim: gpui::Rgba) -> gpui::Div {
    match err {
        Some(msg) => div().text_xs().text_color(
            crate::theme::current().errors.highlight.rgba(),
        ).child(SharedString::from(msg)),
        None => div().text_xs().text_color(dim).child(SharedString::from(
            "Enter saves · Esc cancels · Tab cycles.",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn footer_row(
    save_enabled: bool,
    is_child: bool,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let save_label = if is_child { "Apply" } else { "Save" };
    let save = div()
        .id("ann-save")
        .px_3()
        .py_1p5()
        .rounded_sm()
        .border_1()
        .border_color(if save_enabled { accent } else { border })
        .text_sm()
        .text_color(if save_enabled { fg } else { dim })
        .child(SharedString::from(save_label))
        .when(save_enabled, |d| {
            d.cursor_pointer().on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|shell, _ev, _w, cx| {
                    shell.commit_annotation_frame(cx);
                }),
            )
        });
    let cancel = div()
        .id("ann-cancel")
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
                shell.cancel_annotation_frame(cx);
            }),
        );
    div().flex().flex_row().gap_2().justify_end().child(cancel).child(save)
}

// ----- Key routing --------------------------------------------------

pub fn handle_named_key(shell: &mut Shell, key: &str, cx: &mut Context<Shell>) {
    if let Some(stack) = shell.annotation_stack.as_mut() {
        if let Some(frame) = stack.frames.last_mut() {
            if let Some(input) = frame.focused_input_mut() {
                input.handle_key(key, false, false, false, None, cx);
                cx.notify();
            }
        }
    }
}

pub fn handle_key(shell: &mut Shell, ks: &gpui::Keystroke, cx: &mut Context<Shell>) {
    let key = ks.key.as_str();
    if key == "escape" {
        shell.cancel_annotation_frame(cx);
        return;
    }
    if key == "enter" {
        shell.commit_annotation_frame(cx);
        return;
    }
    if key == "tab" {
        if let Some(stack) = shell.annotation_stack.as_mut() {
            if let Some(frame) = stack.frames.last_mut() {
                frame.cycle_focus(ks.modifiers.shift);
                cx.notify();
            }
        }
        return;
    }
    if let Some(stack) = shell.annotation_stack.as_mut() {
        if let Some(frame) = stack.frames.last_mut() {
            if let Some(input) = frame.focused_input_mut() {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ann() -> SmaliAnnotation {
        SmaliAnnotation {
            visibility: AnnotationVisibility::Runtime,
            annotation_type: TypeSignature::from_jni("Ldagger/Module;"),
            elements: vec![AnnotationElement {
                name: "includes".to_string(),
                value: AnnotationValue::Array(vec!["Lfoo/Bar;".to_string()]),
            }],
        }
    }

    #[test]
    fn round_trips_via_frame() {
        let ann = make_ann();
        let frame = AnnotationFrame::from_annotation(&ann, None);
        let out = frame.to_annotation();
        assert_eq!(out.visibility.to_str(), "runtime");
        assert_eq!(out.annotation_type.to_jni(), "Ldagger/Module;");
        assert_eq!(out.elements.len(), 1);
        assert_eq!(out.elements[0].name, "includes");
        if let AnnotationValue::Array(v) = &out.elements[0].value {
            assert_eq!(v, &vec!["Lfoo/Bar;".to_string()]);
        } else {
            panic!("expected array");
        }
    }

    #[test]
    fn validates_well_formed() {
        let frame = AnnotationFrame::from_annotation(&make_ann(), None);
        assert!(frame.validate().is_ok());
    }

    #[test]
    fn rejects_empty_type() {
        let mut frame = AnnotationFrame::from_annotation(&make_ann(), None);
        frame.annotation_type = TextInput::new();
        assert!(frame.validate().is_err());
    }

    #[test]
    fn rejects_bad_element_name() {
        let mut frame = AnnotationFrame::from_annotation(&make_ann(), None);
        frame.elements[0].name = TextInput::from_text("1bad".to_string());
        assert!(frame.validate().is_err());
    }
}
