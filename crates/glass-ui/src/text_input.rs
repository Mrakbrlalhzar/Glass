//! Reusable single-line text input widget.
//!
//! Owns its own `text`, `cursor`, and `selection_anchor`. Consumers
//! embed a `TextInput` as a field on their state and call
//! `handle_key` from a `on_key_down` listener, then `render` from
//! their render method. The widget emits no actions — consumers
//! decide what to do with the text after each keystroke by
//! reading `text()`.
//!
//! v1 features:
//! - Insertion cursor; Left/Right/Home/End movement.
//! - Selection via Shift+Left/Right/Home/End. Typing replaces
//!   selection. Click + drag is not supported in v1 because
//!   the bare `div` renderer doesn't expose per-character hit
//!   positions in this gpui revision.
//! - Clipboard: ⌘A (select all), ⌘C (copy), ⌘X (cut), ⌘V
//!   (paste / replace selection at cursor).
//! - Backspace deletes selection if any, else char before cursor.
//!   Delete deletes selection if any, else char after cursor.
//!
//! Deferred: word-jump (⌥-arrows), drag-select, undo, IME
//! marked-text rendering.
//!
//! ## Cursor positions
//!
//! `cursor` is a **byte** offset into `text`. The widget operates
//! on UTF-8 char boundaries — moving the cursor walks to the next
//! / previous char boundary so multi-byte characters never split.
//! `selection_anchor`, when `Some`, is the *other* end of the
//! selection; the selected range is `min(cursor, anchor) ..
//! max(cursor, anchor)`. When `None`, no selection is active.

use gpui::{div, px, App, ParentElement, Rgba, SharedString, Styled};

/// Single-line text-editing state + key handlers.
#[derive(Debug, Clone)]
pub struct TextInput {
    text: String,
    /// Byte offset of the insertion point. Always at a UTF-8 char
    /// boundary.
    cursor: usize,
    /// When `Some`, the other end of an active selection. The
    /// selected byte range is `min(cursor, anchor) .. max(...)`.
    selection_anchor: Option<usize>,
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

impl TextInput {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            selection_anchor: None,
        }
    }

    #[allow(dead_code)]
    pub fn from_text(s: impl Into<String>) -> Self {
        let text = s.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            selection_anchor: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the insertion point. Used by external
    /// autocomplete classifiers that need to know where in the
    /// buffer the cursor sits.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Select the entire buffer; cursor lands at the end. Used
    /// when opening an edit pre-populated with existing text so
    /// the user's first keystroke replaces it.
    pub fn select_all_pub(&mut self) {
        self.select_all();
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replace the buffer outright. Cursor lands at the end.
    /// Clears any selection.
    pub fn set_text(&mut self, s: impl Into<String>) {
        self.text = s.into();
        self.cursor = self.text.len();
        self.selection_anchor = None;
    }

    /// Clear the buffer and cursor.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = None;
    }

    // ---- Key handling ---------------------------------------

    /// Handle one key-down event. Returns `true` if the event
    /// mutated the text (so the caller can re-run any derived
    /// computations: search results, candidate lists, etc.).
    ///
    /// `key` should be the `key` field from `gpui::Keystroke`;
    /// `shift`, `cmd`, and `alt` flags ride along separately.
    /// `key_char` is the printable form when the keystroke
    /// produces one (gpui exposes this via `Keystroke::key_char`).
    pub fn handle_key(
        &mut self,
        key: &str,
        shift: bool,
        cmd: bool,
        _alt: bool,
        key_char: Option<&str>,
        cx: &mut App,
    ) -> bool {
        // Clipboard chords first (cmd-A/C/X/V on macOS, ctrl on
        // other platforms — but we collapse both into `cmd` per
        // the listener that calls us).
        if cmd {
            match key {
                "a" => {
                    self.select_all();
                    return false;
                }
                "c" => {
                    self.copy(cx);
                    return false;
                }
                "x" => {
                    return self.cut(cx);
                }
                "v" => {
                    return self.paste(cx);
                }
                _ => return false,
            }
        }

        match key {
            "left" => {
                self.move_cursor(-1, shift);
                false
            }
            "right" => {
                self.move_cursor(1, shift);
                false
            }
            "home" => {
                self.set_cursor(0, shift);
                false
            }
            "end" => {
                let n = self.text.len();
                self.set_cursor(n, shift);
                false
            }
            "backspace" => self.delete_left(),
            "delete" => self.delete_right(),
            _ => {
                if let Some(s) = key_char {
                    if !s.is_empty() {
                        self.insert_str(s);
                        return true;
                    }
                }
                false
            }
        }
    }

    // ---- Editing primitives ---------------------------------

    fn insert_str(&mut self, s: &str) {
        let (a, b) = self.selection_range();
        if a != b {
            self.text.replace_range(a..b, s);
            self.cursor = a + s.len();
        } else {
            self.text.insert_str(self.cursor, s);
            self.cursor += s.len();
        }
        self.selection_anchor = None;
    }

    fn delete_left(&mut self) -> bool {
        let (a, b) = self.selection_range();
        if a != b {
            self.text.replace_range(a..b, "");
            self.cursor = a;
            self.selection_anchor = None;
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        // Walk back to the previous char boundary so we don't
        // split a multi-byte character.
        let mut new_cursor = self.cursor;
        loop {
            new_cursor -= 1;
            if new_cursor == 0 || self.text.is_char_boundary(new_cursor) {
                break;
            }
        }
        self.text.replace_range(new_cursor..self.cursor, "");
        self.cursor = new_cursor;
        true
    }

    fn delete_right(&mut self) -> bool {
        let (a, b) = self.selection_range();
        if a != b {
            self.text.replace_range(a..b, "");
            self.cursor = a;
            self.selection_anchor = None;
            return true;
        }
        if self.cursor >= self.text.len() {
            return false;
        }
        let mut new_end = self.cursor + 1;
        while new_end < self.text.len() && !self.text.is_char_boundary(new_end) {
            new_end += 1;
        }
        self.text.replace_range(self.cursor..new_end, "");
        true
    }

    fn move_cursor(&mut self, delta: i32, shift: bool) {
        let target = if delta > 0 {
            self.next_boundary(self.cursor)
        } else {
            self.prev_boundary(self.cursor)
        };
        self.set_cursor(target, shift);
    }

    fn set_cursor(&mut self, pos: usize, shift: bool) {
        let pos = pos.min(self.text.len());
        if shift {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = pos;
    }

    fn prev_boundary(&self, pos: usize) -> usize {
        if pos == 0 {
            return 0;
        }
        let mut p = pos - 1;
        while p > 0 && !self.text.is_char_boundary(p) {
            p -= 1;
        }
        p
    }

    fn next_boundary(&self, pos: usize) -> usize {
        let n = self.text.len();
        if pos >= n {
            return n;
        }
        let mut p = pos + 1;
        while p < n && !self.text.is_char_boundary(p) {
            p += 1;
        }
        p
    }

    fn selection_range(&self) -> (usize, usize) {
        match self.selection_anchor {
            Some(a) if a != self.cursor => {
                let lo = a.min(self.cursor);
                let hi = a.max(self.cursor);
                (lo, hi)
            }
            _ => (self.cursor, self.cursor),
        }
    }

    fn select_all(&mut self) {
        if self.text.is_empty() {
            return;
        }
        self.selection_anchor = Some(0);
        self.cursor = self.text.len();
    }

    fn copy(&self, cx: &mut App) {
        let (a, b) = self.selection_range();
        if a == b {
            return;
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.text[a..b].to_string()));
    }

    fn cut(&mut self, cx: &mut App) -> bool {
        let (a, b) = self.selection_range();
        if a == b {
            return false;
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.text[a..b].to_string()));
        self.text.replace_range(a..b, "");
        self.cursor = a;
        self.selection_anchor = None;
        true
    }

    fn paste(&mut self, cx: &mut App) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let Some(s) = item.text() else { return false };
        if s.is_empty() {
            return false;
        }
        // Strip newlines — this is a single-line editor.
        let cleaned: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        if cleaned.is_empty() {
            return false;
        }
        self.insert_str(&cleaned);
        true
    }

    // ---- Render ---------------------------------------------

    /// Render the input as three spans (pre-selection,
    /// selection, post-selection) with a vertical caret bar
    /// at the cursor position. Caller wraps in whatever
    /// container they want (gives full control over height,
    /// padding, font, etc.).
    ///
    /// `text_colour` is the colour for normal text; `dim` is
    /// used for the placeholder when the field is empty.
    /// `placeholder` is shown (in `dim`) only when the buffer
    /// is empty.
    /// Multi-line render: text wraps onto multiple visual lines
    /// when it exceeds `wrap_chars` per line. Hard `\n` in the
    /// buffer also break lines. Cursor + selection follow the
    /// wrapped layout. Returns a column of line rows.
    ///
    /// Intended for string-edit popovers where the content can
    /// be 100+ chars long. Single-byte hex edits should keep
    /// using `render`.
    pub fn render_multiline(
        &self,
        text_colour: Rgba,
        dim: Rgba,
        placeholder: &str,
        font: &'static str,
        wrap_chars: usize,
    ) -> gpui::Div {
        use gpui::prelude::*;
        let t = crate::theme::current();
        let field_bg: Rgba = t.hex.field_bg.rgba();
        let sel_bg: Rgba = t.hex.field_selection.rgba();
        let (sel_a, sel_b) = self.selection_range();
        let caret_idx = self.cursor;
        let mut col = div()
            .flex()
            .flex_col()
            .px_2()
            .py_1()
            .bg(field_bg);
        if self.text.is_empty() {
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(caret(text_colour))
                    .child(
                        div()
                            .text_color(dim)
                            .font_family(font)
                            .ml(px(2.))
                            .child(SharedString::from(placeholder.to_string())),
                    ),
            );
            return col;
        }
        // Split the buffer into visual lines. Walk char-by-char
        // tracking byte offsets so we can position the cursor
        // and selection by line + intra-line range.
        let lines = wrap_lines(&self.text, wrap_chars);
        for (line_start, line_end, _is_hard_break_after) in lines {
            let mut line_row = div().flex().flex_row().items_center();
            // Carve into spans at sel_a / sel_b / caret_idx that
            // fall within this line, plus the line endpoints.
            let mut breaks: Vec<usize> = vec![line_start, line_end];
            for &p in &[sel_a, sel_b, caret_idx] {
                if p >= line_start && p <= line_end {
                    breaks.push(p);
                }
            }
            breaks.sort();
            breaks.dedup();
            let mut last = line_start;
            for &b in breaks.iter().skip(1) {
                if b == last {
                    continue;
                }
                let slice = &self.text[last..b];
                let in_sel = last >= sel_a && b <= sel_b && sel_a != sel_b;
                let mut span = div()
                    .font_family(font)
                    .text_color(text_colour)
                    .child(SharedString::from(slice.to_string()));
                if in_sel {
                    span = span.bg(sel_bg);
                }
                line_row = line_row.child(span);
                if b == caret_idx {
                    line_row = line_row.child(caret(text_colour));
                }
                last = b;
            }
            // Caret at end of line when the cursor sits on the
            // synthetic line-end boundary.
            if caret_idx == line_end && !breaks.contains(&caret_idx) {
                line_row = line_row.child(caret(text_colour));
            }
            col = col.child(line_row);
        }
        col
    }

    pub fn render(
        &self,
        text_colour: Rgba,
        dim: Rgba,
        placeholder: &str,
        font: &'static str,
    ) -> gpui::Div {
        let (sel_a, sel_b) = self.selection_range();
        let caret_idx = self.cursor;
        // Subtle field tint — a desaturated version of the
        // tab-selection blue. Makes the input read as a chrome
        // affordance even when empty.
        let field_bg: Rgba = crate::theme::current().hex.field_bg.rgba();
        // Slices: pre [0..min(sel_a, caret)], sel [sel_a..sel_b],
        // post [sel_b..end]. The caret position is rendered as a
        // 1px bar between two spans. To keep things simple we
        // split on cursor and (optionally) selection bounds.
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(22.))
            .px_2()
            .bg(field_bg);
        if self.text.is_empty() {
            // Caret at position 0 — sits to the left of the
            // placeholder so the user sees where typing will land.
            row = row.child(caret(text_colour)).child(
                div()
                    .text_color(dim)
                    .font_family(font)
                    .ml(px(2.))
                    .child(SharedString::from(placeholder.to_string())),
            );
            return row;
        }

        // Build the three text spans, ordering caret + selection
        // inserts inline.
        let sel_bg: Rgba = crate::theme::current().hex.field_selection.rgba();
        // Indices where we need to break: 0, sel_a, sel_b, cursor, end.
        let mut breaks = vec![0usize, self.text.len(), sel_a, sel_b, caret_idx];
        breaks.sort();
        breaks.dedup();
        let mut last = 0usize;
        for &b in breaks.iter().skip(1) {
            if b == last {
                continue;
            }
            let slice = &self.text[last..b];
            let in_sel = last >= sel_a && b <= sel_b && sel_a != sel_b;
            let mut span = div()
                .font_family(font)
                .text_color(text_colour)
                .child(SharedString::from(slice.to_string()));
            if in_sel {
                span = span.bg(sel_bg);
            }
            row = row.child(span);
            if b == caret_idx {
                row = row.child(caret(text_colour));
            }
            last = b;
        }
        row
    }
}

fn caret(colour: Rgba) -> gpui::Div {
    div().w(px(1.)).h(px(14.)).bg(colour).flex_shrink_0()
}

/// Split `text` into visual lines for `render_multiline`. Hard
/// `\n` always ends a line. Within a hard line, wrap when the
/// char count reaches `max_chars`. Returns `(byte_start,
/// byte_end_exclusive, hard_break_after)`. `byte_end` of one
/// line equals `byte_start` of the next (the `\n` itself isn't
/// owned by either line — it's consumed by the implicit break).
fn wrap_lines(text: &str, max_chars: usize) -> Vec<(usize, usize, bool)> {
    let mut out: Vec<(usize, usize, bool)> = Vec::new();
    let mut line_start = 0usize;
    let mut char_count_in_line = 0usize;
    let mut byte = 0usize;
    for ch in text.chars() {
        let ch_len = ch.len_utf8();
        if ch == '\n' {
            out.push((line_start, byte, true));
            line_start = byte + ch_len;
            char_count_in_line = 0;
            byte += ch_len;
            continue;
        }
        char_count_in_line += 1;
        byte += ch_len;
        if char_count_in_line >= max_chars {
            out.push((line_start, byte, false));
            line_start = byte;
            char_count_in_line = 0;
        }
    }
    if line_start <= text.len() {
        out.push((line_start, text.len(), false));
    }
    if out.is_empty() {
        out.push((0, 0, false));
    }
    out
}
