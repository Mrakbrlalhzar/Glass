//! Per-op inline editor for method bodies.
//!
//! When the user double-clicks a row inside a `.method … .end method`
//! block, the row's text is replaced by an in-place `TextInput`.
//! Enter parses the line and stages it as a single-op edit; Esc
//! cancels; Cmd-Enter / Ctrl-Enter inserts a blank op below and
//! moves the editor onto it.
//!
//! How the edit is staged:
//!   * Take the staged-or-original `SmaliMethod`'s `to_smali()`
//!     output.
//!   * Replace the single line at the user's offset with the
//!     newly-typed text (or insert a new blank line for the
//!     Cmd-Enter "new op below" path).
//!   * Wrap the resulting body in a synthetic single-method
//!     `SmaliClass` and re-parse via `SmaliClass::from_smali`.
//!   * Pull the rebuilt ops list out and assign it back to the
//!     real class's method.
//!   * Stage through the existing `stage_smali_class_edit` path
//!     so re-render + export keep working.
//!
//! Why round-trip the whole method rather than parse one line:
//! the smali crate's single-line parser is `pub(crate)` only.
//! Round-tripping is more code than we'd like but is correct
//! against the exact same writer the GUI renders from, so an
//! edited line that produced a valid smali method is guaranteed
//! to round-trip cleanly.

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};

use crate::text_input::TextInput;
use crate::Shell;

// NB: Context<Shell> is referenced by the key-routing helpers
// below; AnyElement / App / SharedString are used by render_row.

/// State for an in-place op edit. Lives on `Shell.op_edit` from
/// the moment the user opens the editor until they Enter / Esc /
/// click outside.
#[derive(Clone)]
pub struct OpEditState {
    pub artifact: glass_db::ArtifactId,
    pub class_jni: String,
    pub method_name: String,
    pub method_signature_jni: String,
    /// Absolute row index in the active tab's `lines` cache —
    /// this is the row the editor replaces visually.
    pub row_index: usize,
    /// Line offset *within* the method's `to_smali()` output.
    /// `0` is the `.method …` header line, which we never let
    /// the user click into (the method-header popover handles
    /// that). Body lines are >= 1.
    pub line_offset_within_method: usize,
    /// Whether this row is a fresh insertion (no original line
    /// to restore on Esc). Affects the "Cancel" behaviour and
    /// the placeholder text.
    pub is_new_line: bool,
    pub input: TextInput,
    /// Last parse-error message, if any. Surfaced inline next
    /// to the editor; cleared on the next keystroke.
    pub error: Option<String>,
    /// Autocomplete candidates for the cursor's current
    /// position. Refreshed on every keystroke. Empty when no
    /// context matches.
    pub suggestions: Vec<OpSuggestion>,
    /// Index of the highlighted suggestion. Up/Down move it,
    /// Tab inserts.
    pub suggestion_selected: usize,
}

/// One row in the op-edit autocomplete dropdown.
#[derive(Debug, Clone)]
pub struct OpSuggestion {
    /// What the dropdown shows in the primary column.
    pub label: SharedString,
    /// Secondary annotation — e.g. "opcode", "v0..v15",
    /// "internal class".
    pub detail: SharedString,
    /// Text spliced into the input on accept. May differ from
    /// `label` (e.g. method-refs we expand to `Class;->name(sig)ret`
    /// while the label shows the dotted form for readability).
    pub commit_text: String,
    /// Span of the input that should be replaced when the
    /// suggestion is committed — i.e. the partial token under
    /// the cursor. Byte offsets, inclusive lower / exclusive
    /// upper.
    pub replace_range: (usize, usize),
    pub kind: OpSuggestionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpSuggestionKind {
    Opcode,
    Register,
    Type,
    MethodRef,
    FieldRef,
}

impl OpSuggestionKind {
    pub fn header_label(self) -> &'static str {
        match self {
            Self::Opcode => "Opcodes  (Tab to insert)",
            Self::Register => "Registers  (Tab to insert)",
            Self::Type => "Types  (Tab to insert)",
            Self::MethodRef => "Methods  (Tab to insert)",
            Self::FieldRef => "Fields  (Tab to insert)",
        }
    }
}

/// What's at the cursor right now — used to decide which
/// suggestion source to pull from. Returned by
/// [`classify_cursor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpCursorContext {
    /// Cursor is in the first whitespace-separated token; the
    /// user is typing the opcode mnemonic.
    Opcode { partial: String, replace_range: (usize, usize) },
    /// Cursor is in a register slot — anything after the opcode
    /// that starts with `v` or `p` and isn't a class ref.
    Register { partial: String, replace_range: (usize, usize) },
    /// Cursor is in a class / type slot — usually after a
    /// register list or as a standalone arg for opcodes like
    /// `new-instance` / `check-cast`.
    Type { partial: String, replace_range: (usize, usize) },
    /// Cursor is in a method-ref slot — `Class;->name(sig)ret`.
    /// `class_jni` is the class portion already typed (or
    /// `None` when only `->` was typed). `partial` is the
    /// member portion typed so far.
    MethodRef {
        class_jni: Option<String>,
        partial: String,
        replace_range: (usize, usize),
    },
    /// Cursor is in a field-ref slot — `Class;->name:Type`.
    FieldRef {
        class_jni: Option<String>,
        partial: String,
        replace_range: (usize, usize),
    },
    /// Nothing useful to suggest.
    None,
}

/// Look at `text` and `cursor_byte` and decide what kind of
/// completion makes sense here. The classifier is intentionally
/// liberal — false positives just mean a dropdown the user can
/// ignore; false negatives leave them with no help at all.
pub fn classify_cursor(text: &str, cursor_byte: usize) -> OpCursorContext {
    let before = &text[..cursor_byte.min(text.len())];
    // Token under cursor: everything since the last whitespace,
    // `,`, `{`, or `(`. (Method-ref `->` is treated specially
    // below since it's two characters.)
    let mut tok_start = 0;
    for (i, ch) in before.char_indices() {
        if matches!(ch, ' ' | '\t' | ',' | '{' | '(') {
            tok_start = i + ch.len_utf8();
        }
    }
    let partial: String = before[tok_start..].to_string();
    let replace_range = (tok_start, cursor_byte);

    // First check whether we've started the opcode yet. If
    // `before` (trimmed) contains no whitespace, the cursor is
    // still in the first token — that's the opcode slot.
    let leading_trim_start = before.len() - before.trim_start().len();
    if before[leading_trim_start..].find(|c: char| c.is_whitespace()).is_none() {
        return OpCursorContext::Opcode {
            partial,
            replace_range: (tok_start.max(leading_trim_start), cursor_byte),
        };
    }

    // We're past the opcode. Recover it for opcode-specific
    // operand-shape decisions.
    let opcode = before.split_whitespace().next().unwrap_or("");

    // Method / field reference: `Class;->member…` — when the
    // partial contains `->`, split there.
    if let Some(arrow) = partial.find("->") {
        let class_part = &partial[..arrow];
        let member_part = &partial[arrow + 2..];
        let class_jni = if class_part.is_empty() {
            None
        } else {
            Some(class_part.to_string())
        };
        let member_start = tok_start + arrow + 2;
        let member_range = (member_start, cursor_byte);
        if opcode_takes_field_ref(opcode) {
            return OpCursorContext::FieldRef {
                class_jni,
                partial: member_part.to_string(),
                replace_range: member_range,
            };
        }
        return OpCursorContext::MethodRef {
            class_jni,
            partial: member_part.to_string(),
            replace_range: member_range,
        };
    }

    // Partial starts with `L` (and isn't a register-style `L0`,
    // which doesn't exist) → it's a class JNI ref the user is
    // typing.
    if partial.starts_with('L') {
        return OpCursorContext::Type { partial, replace_range };
    }

    // `v0`..`v15` / `p0`..`p7` register names.
    if looks_like_register(&partial) {
        return OpCursorContext::Register { partial, replace_range };
    }

    // Empty partial + opcode known + opcode wants a type/method/
    // field in this slot → suggest the right thing even before
    // the user types a character. Picking the "right slot" by
    // operand index requires a per-opcode grammar; for v1 we
    // default-to-type for known type-only opcodes, default-to-
    // register for everything else, and default-to-empty when
    // the cursor sits in something we can't classify.
    if partial.is_empty() {
        if opcode_takes_type_arg(opcode) {
            return OpCursorContext::Type {
                partial,
                replace_range,
            };
        }
        // Most other opcodes take registers as their next slot.
        return OpCursorContext::Register {
            partial,
            replace_range,
        };
    }

    OpCursorContext::None
}

fn looks_like_register(s: &str) -> bool {
    if s.len() < 2 {
        return s == "v" || s == "p";
    }
    let bytes = s.as_bytes();
    if bytes[0] != b'v' && bytes[0] != b'p' {
        return false;
    }
    bytes[1..].iter().all(|b| b.is_ascii_digit())
}

/// Opcodes whose operand is a *field* reference (`Class;->name:Type`)
/// rather than a method reference. Used by the cursor classifier
/// to decide which list to consult after the `->`.
fn opcode_takes_field_ref(opcode: &str) -> bool {
    matches!(
        opcode,
        "iget" | "iget-wide" | "iget-object" | "iget-boolean"
        | "iget-byte" | "iget-char" | "iget-short"
        | "iput" | "iput-wide" | "iput-object" | "iput-boolean"
        | "iput-byte" | "iput-char" | "iput-short"
        | "sget" | "sget-wide" | "sget-object" | "sget-boolean"
        | "sget-byte" | "sget-char" | "sget-short"
        | "sput" | "sput-wide" | "sput-object" | "sput-boolean"
        | "sput-byte" | "sput-char" | "sput-short"
    )
}

/// Opcodes whose primary argument (besides any registers) is a
/// type. Used to decide what to suggest when the partial is
/// empty.
fn opcode_takes_type_arg(opcode: &str) -> bool {
    matches!(
        opcode,
        "new-instance"
        | "new-array"
        | "check-cast"
        | "instance-of"
        | "const-class"
        | "filled-new-array"
        | "filled-new-array/range"
    )
}

/// Static list of Dalvik mnemonics surfaced in the opcode
/// dropdown. Roughly the set the lifter emits — see
/// `smali::dex::DexOp` for the authoritative list.
pub const OPCODE_LIST: &[&str] = &[
    "nop",
    "move", "move/from16", "move/16",
    "move-wide", "move-wide/from16", "move-wide/16",
    "move-object", "move-object/from16", "move-object/16",
    "move-result", "move-result-wide", "move-result-object",
    "move-exception",
    "return-void", "return", "return-wide", "return-object",
    "const/4", "const/16", "const", "const/high16",
    "const-wide/16", "const-wide/32", "const-wide", "const-wide/high16",
    "const-string", "const-string/jumbo", "const-class",
    "monitor-enter", "monitor-exit",
    "check-cast", "instance-of",
    "array-length",
    "new-instance", "new-array",
    "filled-new-array", "filled-new-array/range",
    "fill-array-data",
    "throw",
    "goto", "goto/16", "goto/32",
    "packed-switch", "sparse-switch",
    "cmpl-float", "cmpg-float", "cmpl-double", "cmpg-double", "cmp-long",
    "if-eq", "if-ne", "if-lt", "if-ge", "if-gt", "if-le",
    "if-eqz", "if-nez", "if-ltz", "if-gez", "if-gtz", "if-lez",
    "aget", "aget-wide", "aget-object", "aget-boolean",
    "aget-byte", "aget-char", "aget-short",
    "aput", "aput-wide", "aput-object", "aput-boolean",
    "aput-byte", "aput-char", "aput-short",
    "iget", "iget-wide", "iget-object", "iget-boolean",
    "iget-byte", "iget-char", "iget-short",
    "iput", "iput-wide", "iput-object", "iput-boolean",
    "iput-byte", "iput-char", "iput-short",
    "sget", "sget-wide", "sget-object", "sget-boolean",
    "sget-byte", "sget-char", "sget-short",
    "sput", "sput-wide", "sput-object", "sput-boolean",
    "sput-byte", "sput-char", "sput-short",
    "invoke-virtual", "invoke-super", "invoke-direct",
    "invoke-static", "invoke-interface",
    "invoke-virtual/range", "invoke-super/range", "invoke-direct/range",
    "invoke-static/range", "invoke-interface/range",
    "invoke-polymorphic", "invoke-polymorphic/range",
    "invoke-custom", "invoke-custom/range",
    "neg-int", "not-int", "neg-long", "not-long",
    "neg-float", "neg-double",
    "int-to-long", "int-to-float", "int-to-double",
    "long-to-int", "long-to-float", "long-to-double",
    "float-to-int", "float-to-long", "float-to-double",
    "double-to-int", "double-to-long", "double-to-float",
    "int-to-byte", "int-to-char", "int-to-short",
    "add-int", "sub-int", "mul-int", "div-int", "rem-int",
    "and-int", "or-int", "xor-int", "shl-int", "shr-int", "ushr-int",
    "add-long", "sub-long", "mul-long", "div-long", "rem-long",
    "and-long", "or-long", "xor-long", "shl-long", "shr-long", "ushr-long",
    "add-float", "sub-float", "mul-float", "div-float", "rem-float",
    "add-double", "sub-double", "mul-double", "div-double", "rem-double",
    "add-int/2addr", "sub-int/2addr", "mul-int/2addr",
    "div-int/2addr", "rem-int/2addr",
    "and-int/2addr", "or-int/2addr", "xor-int/2addr",
    "shl-int/2addr", "shr-int/2addr", "ushr-int/2addr",
    "add-long/2addr", "sub-long/2addr", "mul-long/2addr",
    "div-long/2addr", "rem-long/2addr",
    "and-long/2addr", "or-long/2addr", "xor-long/2addr",
    "shl-long/2addr", "shr-long/2addr", "ushr-long/2addr",
    "add-float/2addr", "sub-float/2addr", "mul-float/2addr",
    "div-float/2addr", "rem-float/2addr",
    "add-double/2addr", "sub-double/2addr", "mul-double/2addr",
    "div-double/2addr", "rem-double/2addr",
    "add-int/lit16", "rsub-int", "mul-int/lit16",
    "div-int/lit16", "rem-int/lit16",
    "and-int/lit16", "or-int/lit16", "xor-int/lit16",
    "add-int/lit8", "rsub-int/lit8", "mul-int/lit8",
    "div-int/lit8", "rem-int/lit8",
    "and-int/lit8", "or-int/lit8", "xor-int/lit8",
    "shl-int/lit8", "shr-int/lit8", "ushr-int/lit8",
];

/// Render the inline edit row. Caller embeds the returned div
/// in place of the normal row when
/// `shell.op_edit.row_index == this row's index`.
pub fn render_row(
    state: &OpEditState,
    bg: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
) -> AnyElement {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(22.))
        .w_full()
        .bg(bg)
        .px_3()
        .text_base()
        .font_family("Courier New")
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(state.input.render(
                    fg,
                    dim,
                    if state.is_new_line {
                        "type a smali op, Enter to commit · Esc cancels · ⌘↵ inserts below"
                    } else {
                        "Enter commits · Esc cancels · ⌘↵ inserts below"
                    },
                    "Courier New",
                )),
        );
    if let Some(err) = state.error.as_ref() {
        row = row.child(
            div()
                .ml_3()
                .text_xs()
                .text_color(crate::theme::current().errors.highlight.rgba())
                .child(SharedString::from(err.clone())),
        );
    }
    row.on_mouse_down(
        gpui::MouseButton::Left,
        |_ev, _w, cx: &mut App| {
            // Eat clicks inside the editor so they don't bubble
            // up to the row's select-on-click handler.
            cx.stop_propagation();
        },
    )
    .into_any_element()
}

/// Build the modified method body by replacing the line at
/// `line_offset_within_method` in `method.to_smali()` with
/// `new_line`. Returns the assembled text ready to feed to
/// the wrapper-parse round-trip.
///
/// `insert_after` means "place the new line *after* the current
/// offset" instead of replacing in place — used by the Cmd-Enter
/// new-line path.
pub fn splice_method_body(
    method_text: &str,
    line_offset: usize,
    new_line: &str,
    insert_after: bool,
) -> String {
    let mut lines: Vec<String> = method_text.lines().map(|s| s.to_string()).collect();
    if insert_after {
        let idx = (line_offset + 1).min(lines.len());
        lines.insert(idx, new_line.to_string());
    } else if line_offset < lines.len() {
        lines[line_offset] = new_line.to_string();
    } else {
        lines.push(new_line.to_string());
    }
    lines.join("\n") + "\n"
}

/// Wrap a method body in a synthetic class so the public
/// `SmaliClass::from_smali` can parse it. Returns the assembled
/// smali text.
///
/// `class_jni` is informational only — the wrapper uses a fixed
/// JNI name so the parser doesn't get tripped up on duplicate
/// class names within a session.
pub fn wrap_in_synthetic_class(method_body: &str, _class_jni: &str) -> String {
    format!(
        ".class public Lglass/internal/OpRoundTrip;\n\
         .super Ljava/lang/Object;\n\
         {method_body}\n"
    )
}

/// Render the floating dropdown listing the current suggestions.
/// Caller embeds the returned element in the root overlay layer
/// when `shell.op_edit` is `Some` and its `suggestions` is
/// non-empty. Positioned absolutely in the top-right, same idiom
/// as the disasm-edit dropdown — anchoring to the actual row in
/// a list view is awkward because the list element doesn't
/// expose its per-row screen coordinates here.
pub fn render_suggestions(
    state: &OpEditState,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    let selected = state.suggestion_selected;
    let header_text = state
        .suggestions
        .first()
        .map(|s| s.kind.header_label())
        .unwrap_or("Suggestions");
    let mut list = div()
        .id("op-edit-suggestions")
        .w(px(420.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_sm()
        .shadow_lg()
        .flex()
        .flex_col()
        .occlude()
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        )
        .child(
            div()
                .px_2()
                .py_1()
                .text_xs()
                .text_color(dim)
                .border_b_1()
                .border_color(border)
                .child(SharedString::from(header_text)),
        );
    for (i, sugg) in state.suggestions.iter().enumerate().take(12) {
        let is_sel = i == selected;
        let theme = crate::theme::current();
        let bg = if is_sel {
            theme.modals.palette_selected.rgba()
        } else {
            gpui::rgba(0x00000000)
        };
        let label_color = if is_sel {
            theme.shell.text_bright.rgba()
        } else {
            theme.shell.text.rgba()
        };
        list = list.child(
            div()
                .id(("op-edit-suggestion-row", i))
                .px_2()
                .py_1()
                .bg(bg)
                .flex()
                .flex_row()
                .gap_3()
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::current().modals.palette_hover.rgba()))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(label_color)
                        .font_family("Courier New")
                        .child(sugg.label.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(crate::theme::current().disasm.address.rgba())
                        .child(sugg.detail.clone()),
                )
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _ev, _w, cx| {
                        this.click_op_edit_suggestion(i, cx);
                    }),
                ),
        );
    }
    div()
        .absolute()
        .top(px(72.))
        .right(px(20.))
        .child(list)
        .into_any_element()
}

/// Keystroke handler. Called from the root `on_key_down` listener
/// when `shell.op_edit.is_some()`. Returns `true` if the event
/// was consumed.
pub fn handle_key(shell: &mut Shell, ks: &gpui::Keystroke, cx: &mut Context<Shell>) {
    let key = ks.key.as_str();
    if key == "escape" {
        // If the dropdown is open, Esc just closes the dropdown
        // rather than the whole editor. Mirrors the palette /
        // disasm-edit idiom: progressive cancellation.
        if shell
            .op_edit
            .as_ref()
            .is_some_and(|e| !e.suggestions.is_empty())
        {
            if let Some(state) = shell.op_edit.as_mut() {
                state.suggestions.clear();
                state.suggestion_selected = 0;
                cx.notify();
            }
            return;
        }
        shell.cancel_op_edit(cx);
        return;
    }
    if key == "enter" {
        if ks.modifiers.platform || ks.modifiers.control {
            shell.commit_op_edit_and_insert_below(cx);
        } else {
            shell.commit_op_edit(cx);
        }
        return;
    }
    if key == "tab" {
        // Tab accepts the current suggestion; if there's nothing
        // to accept it falls through to inserting a tab character,
        // which we don't want inside a smali line. Swallow the key
        // either way.
        shell.accept_op_edit_suggestion(cx);
        return;
    }
    if let Some(state) = shell.op_edit.as_mut() {
        state.input.handle_key(
            key,
            ks.modifiers.shift,
            ks.modifiers.platform || ks.modifiers.control,
            ks.modifiers.alt,
            ks.key_char.as_deref(),
            cx,
        );
        // Any keystroke clears the last error so the inline chip
        // doesn't linger after the user starts typing again.
        state.error = None;
        cx.notify();
    }
    // Refresh suggestions to reflect the new cursor/text. Must
    // run after the input mutation above.
    shell.refresh_op_edit_suggestions(cx);
}

/// Forward a synthetic key (no modifiers) to the input — used
/// by the action handlers that intercept arrow keys before
/// `on_key_down` runs.
///
/// Up/Down move the suggestion selection when the dropdown is
/// open; otherwise (and for left/right) they go to the input.
pub fn handle_named_key(shell: &mut Shell, key: &str, cx: &mut Context<Shell>) {
    let has_suggestions = shell
        .op_edit
        .as_ref()
        .is_some_and(|e| !e.suggestions.is_empty());
    if has_suggestions && (key == "up" || key == "down") {
        if let Some(state) = shell.op_edit.as_mut() {
            let n = state.suggestions.len();
            if n > 0 {
                let cur = state.suggestion_selected;
                state.suggestion_selected = if key == "up" {
                    (cur + n - 1) % n
                } else {
                    (cur + 1) % n
                };
                cx.notify();
            }
        }
        return;
    }
    if let Some(state) = shell.op_edit.as_mut() {
        state.input.handle_key(key, false, false, false, None, cx);
        cx.notify();
    }
    // Cursor moves change context — recompute.
    shell.refresh_op_edit_suggestions(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_opcode_at_start() {
        let ctx = classify_cursor("inv", 3);
        match ctx {
            OpCursorContext::Opcode { partial, .. } => assert_eq!(partial, "inv"),
            other => panic!("expected Opcode, got {other:?}"),
        }
    }

    #[test]
    fn classify_opcode_with_leading_whitespace() {
        let ctx = classify_cursor("    inv", 7);
        assert!(matches!(ctx, OpCursorContext::Opcode { ref partial, .. } if partial == "inv"));
    }

    #[test]
    fn classify_register_after_opcode() {
        let ctx = classify_cursor("move v", 6);
        match ctx {
            OpCursorContext::Register { partial, .. } => assert_eq!(partial, "v"),
            other => panic!("expected Register, got {other:?}"),
        }
    }

    #[test]
    fn classify_type_after_new_instance() {
        let ctx = classify_cursor("new-instance v0, Lja", 20);
        match ctx {
            OpCursorContext::Type { partial, .. } => assert_eq!(partial, "Lja"),
            other => panic!("expected Type, got {other:?}"),
        }
    }

    #[test]
    fn classify_method_ref_after_arrow() {
        let ctx =
            classify_cursor("invoke-virtual {v0}, Lcom/Foo;->bar", 35);
        match ctx {
            OpCursorContext::MethodRef { class_jni, partial, .. } => {
                assert_eq!(class_jni.as_deref(), Some("Lcom/Foo;"));
                assert_eq!(partial, "bar");
            }
            other => panic!("expected MethodRef, got {other:?}"),
        }
    }

    #[test]
    fn classify_field_ref_after_iget_arrow() {
        let ctx = classify_cursor("iget v0, p0, Lcom/Foo;->count", 29);
        match ctx {
            OpCursorContext::FieldRef { class_jni, partial, .. } => {
                assert_eq!(class_jni.as_deref(), Some("Lcom/Foo;"));
                assert_eq!(partial, "count");
            }
            other => panic!("expected FieldRef, got {other:?}"),
        }
    }

    #[test]
    fn splice_replaces_in_place() {
        let body =
            ".method public foo()V\n    return-void\n.end method\n";
        let out = splice_method_body(body, 1, "    nop", false);
        assert!(out.contains("    nop\n"));
        assert!(!out.contains("    return-void"));
    }

    #[test]
    fn splice_inserts_after() {
        let body =
            ".method public foo()V\n    nop\n    return-void\n.end method\n";
        let out = splice_method_body(body, 1, "    nop", true);
        let lines: Vec<&str> = out.lines().collect();
        // After insertion: header, nop, nop (new), return-void, end.
        assert_eq!(lines[0].trim(), ".method public foo()V");
        assert_eq!(lines[1].trim(), "nop");
        assert_eq!(lines[2].trim(), "nop");
        assert_eq!(lines[3].trim(), "return-void");
    }
}
