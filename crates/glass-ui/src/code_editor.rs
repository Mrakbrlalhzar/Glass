//! Rope-backed code editor widget — the foundation of script
//! editing in Glass, and (eventually) a replacement for the
//! popover-per-method smali editor.
//!
//! Built on Zed's `text::Buffer` (rope storage, anchors,
//! transactional edits, undo/redo). The renderer is Glass-native:
//! it owns a virtualized list of line rows, a small gutter for
//! line numbers, and integrates with our theme + scrollbar. No
//! workspace / project / client dependency — just `text`, `rope`,
//! and (later) `language` for syntax highlighting.
//!
//! ## What's here today
//!
//! - `CodeEditor` state: holds the `Buffer`, the file name, and a
//!   `dirty` flag.
//! - `render_code_editor` — renders the buffer line-by-line with a
//!   line-number gutter and our standard `list_scrollbar`.
//!
//! ## Coming next
//!
//! - Cursor + selection, keyboard input → buffer edits, undo/redo.
//! - Cmd-S save (driver lives in `scripts_actions::save_script_body`).
//! - Copy / paste via gpui clipboard.
//! - Find-in-buffer (Cmd-F).
//! - Tree-sitter syntax highlighting via the `language` registry.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    div, list, prelude::*, px, App, Context, ListAlignment, ListState, SharedString,
};

use crate::Shell;

/// Monotonic source of `BufferId`s within the process. `text::Buffer`
/// uses these to dedupe operations in a collaborative-editing world;
/// Glass is single-user so any unique non-zero value will do.
fn next_buffer_id() -> text::BufferId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    text::BufferId::from(NonZeroU64::new(n).expect("buffer id overflow"))
}

/// Per-tab editor state. One of these per open `TabKind::ScriptEditor`
/// — they share nothing, so closing a tab cleans up by drop.
pub(crate) struct CodeEditor {
    /// The underlying rope-backed buffer. Mutated through edit
    /// operations; read through `buffer.snapshot()`.
    pub buffer: text::Buffer,
    /// Virtualized list state — one row per visual line. Refreshed
    /// (length only) on every edit; row content streams from the
    /// buffer at render time.
    pub list_state: ListState,
    /// Whether the buffer has unsaved edits. Set on any edit,
    /// cleared by the save flow.
    pub dirty: bool,
    /// Cached line count of the buffer's current snapshot. Used
    /// to size the list + the line-number gutter width.
    cached_row_count: usize,
    /// Caret offset, in bytes. `0..=buffer.len()`. Selection lives
    /// between `selection_anchor` (the side fixed when a drag /
    /// shift-extend started) and `cursor`; when they differ the
    /// renderer draws a highlight between them.
    cursor: usize,
    /// Other end of the selection. `None` means no selection (just
    /// the caret at `cursor`).
    selection_anchor: Option<usize>,
    /// "Sticky" target column for vertical motion. When the user
    /// presses Up/Down from a long line to a short one and back,
    /// we want to land back on the original column. Reset on any
    /// non-vertical motion.
    desired_column: Option<u32>,
}

impl CodeEditor {
    /// Build an editor from `text`. The buffer normalises line
    /// endings to `\n` internally; we keep the user-visible end-of-
    /// file behaviour identical to what they typed.
    pub fn from_string(text: impl Into<String>) -> Self {
        let buffer = text::Buffer::new(
            text::ReplicaId::LOCAL,
            next_buffer_id(),
            text,
        );
        let row_count = buffer.snapshot().row_count() as usize;
        Self {
            buffer,
            list_state: ListState::new(row_count, ListAlignment::Top, px(2000.)),
            dirty: false,
            cached_row_count: row_count,
            cursor: 0,
            selection_anchor: None,
            desired_column: None,
        }
    }

    /// Total bytes in the buffer's current visible text. Used to
    /// clamp cursor motion.
    fn len(&self) -> usize {
        self.buffer.snapshot().len()
    }

    /// Caret byte offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Caret as a `(row, column)` point, in **bytes** for the
    /// column (matching `rope::Point`). The renderer uses this to
    /// draw the caret on the correct line + position.
    pub fn cursor_point(&self) -> rope::Point {
        self.buffer.snapshot().offset_to_point(self.cursor)
    }

    /// Selection as a `(start, end)` byte range. When no selection
    /// is active, returns `(cursor, cursor)`.
    pub fn selection_range(&self) -> (usize, usize) {
        match self.selection_anchor {
            Some(anchor) if anchor != self.cursor => {
                if anchor < self.cursor {
                    (anchor, self.cursor)
                } else {
                    (self.cursor, anchor)
                }
            }
            _ => (self.cursor, self.cursor),
        }
    }

    /// Snapshot the buffer once and refresh the cached line count.
    /// Call after every mutation.
    fn refresh_cache(&mut self) {
        self.cached_row_count = self.buffer.snapshot().row_count() as usize;
        // Resize the list to match. ListState doesn't grow itself.
        self.list_state =
            ListState::new(self.cached_row_count, ListAlignment::Top, px(2000.));
    }

    /// Apply an edit: replace `range` with `new_text`. Advances
    /// the cursor to the end of the inserted text.
    fn apply_edit(&mut self, range: std::ops::Range<usize>, new_text: &str) {
        let new_len = new_text.len();
        let start = range.start;
        self.buffer.edit([(range, new_text)]);
        self.cursor = start + new_len;
        self.selection_anchor = None;
        self.desired_column = None;
        self.dirty = true;
        self.refresh_cache();
    }

    fn set_cursor(&mut self, offset: usize, extend_selection: bool) {
        let clamped = offset.min(self.len());
        if extend_selection {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        if clamped != self.cursor {
            self.cursor = clamped;
            self.desired_column = None;
        }
    }

    /// Move the caret by one character left/right (honouring char
    /// boundaries). Selection extends when `shift` is held.
    fn move_horizontal(&mut self, dir: i32, shift: bool) {
        let snap = self.buffer.snapshot();
        let len = snap.len();
        let target = if dir < 0 {
            // Walk left to the previous char boundary. We work
            // off the snapshot's chars stream rather than rolling
            // our own byte walker — keeps multi-byte UTF-8 safe.
            if self.cursor == 0 {
                0
            } else {
                prev_char_boundary(&snap, self.cursor)
            }
        } else if self.cursor >= len {
            len
        } else {
            next_char_boundary(&snap, self.cursor)
        };
        self.set_cursor(target, shift);
    }

    /// Move the caret one visual line up/down. Tries to land on
    /// `desired_column` (set when vertical motion started); falls
    /// back to the current column otherwise.
    fn move_vertical(&mut self, dir: i32, shift: bool) {
        let snap = self.buffer.snapshot();
        let here = snap.offset_to_point(self.cursor);
        let desired = self.desired_column.unwrap_or(here.column);
        let new_row = (here.row as i64 + dir as i64).max(0) as u32;
        let max_row = snap.max_point().row;
        if new_row > max_row {
            // Past the end — clamp to end of buffer, but remember
            // the column for further verticals.
            let end = snap.len();
            self.set_cursor(end, shift);
            self.desired_column = Some(desired);
            return;
        }
        // Pick min(desired, length of new row in bytes).
        let row_end_col = row_length_bytes(&snap, new_row);
        let col = desired.min(row_end_col);
        let target = snap.point_to_offset(rope::Point::new(new_row, col));
        self.set_cursor(target, shift);
        self.desired_column = Some(desired);
    }

    fn move_line_start(&mut self, shift: bool) {
        let snap = self.buffer.snapshot();
        let here = snap.offset_to_point(self.cursor);
        let target = snap.point_to_offset(rope::Point::new(here.row, 0));
        self.set_cursor(target, shift);
    }

    fn move_line_end(&mut self, shift: bool) {
        let snap = self.buffer.snapshot();
        let here = snap.offset_to_point(self.cursor);
        let col = row_length_bytes(&snap, here.row);
        let target = snap.point_to_offset(rope::Point::new(here.row, col));
        self.set_cursor(target, shift);
    }

    /// Select the entire buffer.
    fn select_all(&mut self) {
        let len = self.len();
        self.selection_anchor = Some(0);
        self.cursor = len;
        self.desired_column = None;
    }

    /// Replace the current selection (or insert at the caret if
    /// there's no selection) with `text`. Caret ends up at the
    /// end of the inserted text.
    fn insert_str(&mut self, text: &str) {
        let (a, b) = self.selection_range();
        self.apply_edit(a..b, text);
    }

    fn delete_left(&mut self) {
        let (a, b) = self.selection_range();
        if a != b {
            self.apply_edit(a..b, "");
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let snap = self.buffer.snapshot();
        let prev = prev_char_boundary(&snap, self.cursor);
        self.apply_edit(prev..self.cursor, "");
    }

    fn delete_right(&mut self) {
        let (a, b) = self.selection_range();
        if a != b {
            self.apply_edit(a..b, "");
            return;
        }
        let len = self.len();
        if self.cursor >= len {
            return;
        }
        let snap = self.buffer.snapshot();
        let next = next_char_boundary(&snap, self.cursor);
        self.apply_edit(self.cursor..next, "");
    }

    /// Handle a keystroke. Returns true when the buffer changed
    /// (so the caller can flush dirty state, repaint, etc.).
    pub fn handle_key(
        &mut self,
        key: &str,
        shift: bool,
        cmd: bool,
        key_char: Option<&str>,
    ) -> bool {
        if cmd {
            match key {
                "a" => {
                    self.select_all();
                    return false;
                }
                // c/x/v handled by caller (system clipboard);
                // s/f also caller-handled.
                _ => return false,
            }
        }
        match key {
            "left" => {
                self.move_horizontal(-1, shift);
                false
            }
            "right" => {
                self.move_horizontal(1, shift);
                false
            }
            "up" => {
                self.move_vertical(-1, shift);
                false
            }
            "down" => {
                self.move_vertical(1, shift);
                false
            }
            "home" => {
                self.move_line_start(shift);
                false
            }
            "end" => {
                self.move_line_end(shift);
                false
            }
            "backspace" => {
                self.delete_left();
                true
            }
            "delete" => {
                self.delete_right();
                true
            }
            "enter" => {
                self.insert_str("\n");
                true
            }
            "tab" => {
                // 4-space soft tab; matches our smali / disasm
                // editor convention. Real tab handling (insert
                // literal \t, indent-on-selection) can land
                // later.
                self.insert_str("    ");
                true
            }
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

    /// Read the buffer as a `String`. Used by the save flow.
    pub fn text(&self) -> String {
        self.buffer.snapshot().text()
    }

    /// Total visual lines — used for the line-number gutter width
    /// and the list size.
    pub fn line_count(&self) -> usize {
        self.cached_row_count
    }

    /// Width of the gutter, in pixels, sized to fit the largest
    /// line number plus a small inset. 4 chars minimum so the
    /// gutter doesn't visually jitter as you scroll into bigger
    /// numbers.
    pub fn gutter_width_px(&self) -> f32 {
        let n_digits = digit_count(self.line_count().max(1)).max(4) as f32;
        // ~7px per digit in our small fixed-width font + 12px inset.
        n_digits * 7.5 + 12.0
    }
}

/// Length of the given row in **bytes**, excluding the trailing
/// newline. Used for cursor clamping on vertical motion + line-end.
fn row_length_bytes(snap: &text::BufferSnapshot, row: u32) -> u32 {
    let max = snap.max_point();
    if row >= max.row {
        return max.column;
    }
    // Length = offset(row+1, 0) - offset(row, 0) - 1 (the \n).
    let start = snap.point_to_offset(rope::Point::new(row, 0));
    let end = snap.point_to_offset(rope::Point::new(row + 1, 0));
    (end - start).saturating_sub(1) as u32
}

/// Walk the buffer's rope to the previous valid char boundary
/// before `offset`. Multi-byte UTF-8 safe.
fn prev_char_boundary(snap: &text::BufferSnapshot, offset: usize) -> usize {
    let mut o = offset;
    while o > 0 {
        o -= 1;
        if snap.as_rope().is_char_boundary(o) {
            return o;
        }
    }
    0
}

/// Walk the buffer's rope to the next valid char boundary at or
/// after `offset + 1`.
fn next_char_boundary(snap: &text::BufferSnapshot, offset: usize) -> usize {
    let len = snap.len();
    let mut o = offset + 1;
    while o < len {
        if snap.as_rope().is_char_boundary(o) {
            return o;
        }
        o += 1;
    }
    len
}

fn digit_count(n: usize) -> usize {
    let mut n = n;
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Render the editor as a flex column: gutter on the left, body on
/// the right, scrollbar overlaid. The caller drops this into the
/// active-tab body slot in `two_pane.rs`.
pub fn render_code_editor(
    editor: &CodeEditor,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    let _ = cx;
    let theme = crate::theme::current();
    let gutter_w = px(editor.gutter_width_px());
    // Snapshot the buffer once per render; `list` holds the
    // closure for the lifetime of the visible-row callbacks, so
    // we own an `Arc<BufferSnapshot>` (the underlying rope is
    // already Arc-internally; cloning a snapshot is cheap).
    let snapshot = std::sync::Arc::new(editor.buffer.snapshot().clone());
    let row_count = editor.cached_row_count;
    // Caret + selection state — captured so each row can decide
    // whether to draw a caret / highlight a segment.
    let cursor_point = editor.cursor_point();
    let (sel_start_off, sel_end_off) = editor.selection_range();
    let selection: Option<(rope::Point, rope::Point)> = if sel_start_off == sel_end_off {
        None
    } else {
        Some((
            snapshot.offset_to_point(sel_start_off),
            snapshot.offset_to_point(sel_end_off),
        ))
    };
    let caret_colour = theme.shell.text_bright.rgba();
    let selection_colour = theme.modals.palette_hover.rgba();

    // gpui's list takes the list_state by value; we clone here so
    // the editor keeps owning its copy.
    let list_state = editor.list_state.clone();

    let body = list(list_state, {
        let snapshot = snapshot.clone();
        move |index, _window, _cx| {
            // Pull the line text from the rope. `Lines::next` is a
            // streaming iterator; we advance to `index` then take
            // the next line. This is O(line index) per render, but
            // the virtualized list only fetches visible rows, so
            // typical N is <100. For very large files we'll cache
            // line offsets later.
            let line_text = nth_line(&snapshot, index);
            let line_no = index + 1;
            let line_no_str = SharedString::from(format!("{line_no}"));
            let row = index as u32;
            // Build the body span as a chunk row so we can layer
            // selection highlight + caret without resorting to
            // absolute positioning. Three spans:
            //   * before the caret/selection start
            //   * the selection range (or just the caret)
            //   * after
            let body_el = render_line_body(
                &line_text,
                row,
                cursor_point,
                selection,
                fg,
                caret_colour,
                selection_colour,
            );
            div()
                .h(px(LINE_HEIGHT))
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .child(
                    // Right-aligned line-number gutter, dim text,
                    // bordered on the right to separate from the
                    // body.
                    div()
                        .w(gutter_w)
                        .flex_shrink_0()
                        .h_full()
                        .pr_2()
                        .flex()
                        .items_center()
                        .justify_end()
                        .text_xs()
                        .text_color(dim)
                        .font_family(EDITOR_FONT)
                        .child(line_no_str),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .pl_2()
                        .h_full()
                        .text_xs()
                        .font_family(EDITOR_FONT)
                        .child(body_el),
                )
                .into_any()
        }
    });

    let scrollbar =
        crate::scrollbar::list_scrollbar(&editor.list_state, border, dim);

    div()
        .size_full()
        .bg(panel)
        .relative()
        .child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(body.flex_1())
                .child(
                    // Footer chip: line count + dirty indicator.
                    // Tiny — just enough to confirm "this is the
                    // editor pane" while we have no other affordances.
                    div()
                        .h(px(20.))
                        .w_full()
                        .px_3()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .text_xs()
                        .text_color(dim)
                        .bg(theme.shell.panel.rgba())
                        .border_t_1()
                        .border_color(border)
                        .child(SharedString::from(format!(
                            "{row_count} lines"
                        )))
                        .child(SharedString::from(if editor.dirty {
                            "● modified"
                        } else {
                            "saved"
                        })),
                ),
        )
        .child(scrollbar)
        .into_any_element()
}

/// Build the body element for one row: text + (optional)
/// selection-highlight overlay + (optional) caret. Rendering is
/// span-based so the highlight tints only the selected bytes,
/// not the whole row.
fn render_line_body(
    text: &str,
    row: u32,
    cursor: rope::Point,
    selection: Option<(rope::Point, rope::Point)>,
    fg: gpui::Rgba,
    caret_colour: gpui::Rgba,
    selection_colour: gpui::Rgba,
) -> gpui::Div {
    // Convert per-row byte columns to char indices for slicing
    // (the rope columns are bytes; rust slices need to be on
    // char boundaries).
    let line_len_bytes = text.len();
    // Selection range for this row, in byte columns. None when
    // the selection doesn't touch this row.
    let row_sel = selection.and_then(|(start, end)| {
        if row < start.row || row > end.row {
            return None;
        }
        let s = if row == start.row { start.column as usize } else { 0 };
        let e = if row == end.row { end.column as usize } else { line_len_bytes };
        Some((s.min(line_len_bytes), e.min(line_len_bytes)))
    });
    let caret_col = if row == cursor.row {
        Some((cursor.column as usize).min(line_len_bytes))
    } else {
        None
    };
    // Compose a flex row of spans. We always emit at least one
    // child so empty lines still register a row height.
    let mut row_el = gpui::prelude::FluentBuilder::when(
        gpui::div(),
        true,
        |d| d,
    )
    .flex()
    .flex_row()
    .items_center()
    .h_full()
    .text_color(fg)
    // Allow horizontal overflow on long lines — gpui's flex row
    // wraps without this.
    .whitespace_nowrap();
    use gpui::prelude::*;
    if let Some((s, e)) = row_sel.filter(|(s, e)| s < e) {
        let before = safe_slice(text, 0, s);
        let inside = safe_slice(text, s, e);
        let after = safe_slice(text, e, line_len_bytes);
        if !before.is_empty() {
            row_el = row_el.child(gpui::div().child(SharedString::from(before.to_string())));
        }
        row_el = row_el.child(
            gpui::div()
                .bg(selection_colour)
                .child(SharedString::from(inside.to_string())),
        );
        if !after.is_empty() {
            row_el = row_el.child(gpui::div().child(SharedString::from(after.to_string())));
        }
    } else if !text.is_empty() {
        row_el = row_el.child(gpui::div().child(SharedString::from(text.to_string())));
    }
    // Caret: rendered as a 1px-wide div positioned absolutely
    // inside a relative wrapper so it sits on top of the text.
    // For now we approximate horizontal position by the byte
    // column × a fixed glyph width — Courier New at our zoom is
    // close to monospace so this looks right within a pixel.
    if let Some(col) = caret_col {
        let x = col as f32 * GLYPH_WIDTH;
        row_el = row_el.child(
            gpui::div()
                .absolute()
                .left(px(x))
                .top(px(2.))
                .w(px(1.))
                .h(px(LINE_HEIGHT - 4.0))
                .bg(caret_colour),
        );
    }
    // Wrap in a relative container so the absolute-positioned
    // caret has the right reference frame.
    gpui::div()
        .relative()
        .h_full()
        .w_full()
        .child(row_el)
}

/// UTF-8 safe slice — clamps the requested byte range to char
/// boundaries on either side. Returns an empty &str if the
/// range collapses.
fn safe_slice(text: &str, mut start: usize, mut end: usize) -> &str {
    start = start.min(text.len());
    end = end.min(text.len());
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    if start >= end {
        return "";
    }
    &text[start..end]
}

/// Approximate per-glyph width in the editor font, in pixels.
/// Courier New at our `text_xs` size renders close to monospace;
/// tuned by eye to align the caret with the underlying text. If
/// this drifts we'll measure at runtime instead.
const GLYPH_WIDTH: f32 = 7.5;

/// Height of a single editor line. Matches `text_xs` rendering at
/// our usual zoom. Tuned by eye against the listing view's row
/// height.
const LINE_HEIGHT: f32 = 16.0;

/// Editor monospace font. Same family the smali / listing views
/// use so it feels consistent.
const EDITOR_FONT: &str = "Courier New";

/// Fetch the text of the `index`-th line in the snapshot. Returns
/// an empty string if `index` is past the end (the virtualized
/// list can briefly request rows beyond `cached_row_count` during
/// resizes — better to return empty than panic).
fn nth_line(snapshot: &text::BufferSnapshot, index: usize) -> String {
    let target = index as u32;
    // Range over the single line: from `Point(target, 0)` to
    // `Point(target+1, 0)`, clamped to the buffer.
    let max = snapshot.max_point();
    if target > max.row {
        return String::new();
    }
    let start = rope::Point::new(target, 0);
    let end = if target == max.row {
        rope::Point::new(target, max.column)
    } else {
        rope::Point::new(target + 1, 0)
    };
    let start_off = snapshot.point_to_offset(start);
    let end_off = snapshot.point_to_offset(end);
    let mut s = String::with_capacity(end_off.saturating_sub(start_off));
    for chunk in snapshot.as_rope().chunks_in_range(start_off..end_off) {
        s.push_str(chunk);
    }
    // Strip the trailing newline so the row doesn't render an
    // extra blank slot.
    if s.ends_with('\n') {
        s.pop();
    }
    if s.ends_with('\r') {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_string_counts_lines() {
        let e = CodeEditor::from_string("one\ntwo\nthree");
        // 3 lines = row_count of last point + 1 = 3
        assert_eq!(e.line_count(), 3);
    }

    #[test]
    fn from_string_empty() {
        let e = CodeEditor::from_string("");
        assert_eq!(e.line_count(), 1);
    }

    #[test]
    fn nth_line_returns_each_line() {
        let e = CodeEditor::from_string("alpha\nbeta\ngamma");
        let snap = e.buffer.snapshot();
        assert_eq!(nth_line(&snap, 0), "alpha");
        assert_eq!(nth_line(&snap, 1), "beta");
        assert_eq!(nth_line(&snap, 2), "gamma");
        assert_eq!(nth_line(&snap, 3), "");
    }

    #[test]
    fn typing_inserts_at_cursor() {
        let mut e = CodeEditor::from_string("hello");
        // Cursor starts at 0 — type "X" → "Xhello", cursor at 1.
        e.handle_key("x", false, false, Some("X"));
        assert_eq!(e.text(), "Xhello");
        assert_eq!(e.cursor(), 1);
        assert!(e.dirty);
    }

    #[test]
    fn enter_splits_line_and_updates_row_count() {
        let mut e = CodeEditor::from_string("ab");
        // Move to between a and b, then Enter.
        e.handle_key("right", false, false, None);
        e.handle_key("enter", false, false, None);
        assert_eq!(e.text(), "a\nb");
        // Caret should be at start of the new line (row 1, col 0).
        let p = e.cursor_point();
        assert_eq!((p.row, p.column), (1, 0));
        assert_eq!(e.line_count(), 2);
    }

    #[test]
    fn backspace_deletes_left() {
        let mut e = CodeEditor::from_string("abc");
        // Move to end, backspace once → "ab".
        for _ in 0..3 {
            e.handle_key("right", false, false, None);
        }
        e.handle_key("backspace", false, false, None);
        assert_eq!(e.text(), "ab");
        assert_eq!(e.cursor(), 2);
    }

    #[test]
    fn arrow_down_uses_desired_column() {
        // Move down from a long line to a short one and back —
        // caret should return to the original column, not stay
        // clamped at the short line's end.
        let mut e = CodeEditor::from_string("hello world\nhi\nback again");
        // Move to column 7 of row 0 ("hello w|orld").
        for _ in 0..7 {
            e.handle_key("right", false, false, None);
        }
        assert_eq!(e.cursor_point().column, 7);
        // Down — short row, clamps to 2.
        e.handle_key("down", false, false, None);
        assert_eq!(e.cursor_point().row, 1);
        assert_eq!(e.cursor_point().column, 2);
        // Down again — long enough row, should restore column 7.
        e.handle_key("down", false, false, None);
        assert_eq!(e.cursor_point().row, 2);
        assert_eq!(e.cursor_point().column, 7);
    }

    #[test]
    fn select_all_then_type_replaces() {
        let mut e = CodeEditor::from_string("old text");
        e.handle_key("a", false, true, None); // cmd-a
        e.handle_key("n", false, false, Some("N"));
        assert_eq!(e.text(), "N");
        assert_eq!(e.cursor(), 1);
    }

    #[test]
    fn shift_left_extends_selection() {
        let mut e = CodeEditor::from_string("abc");
        // Move to end, then shift-left twice — selection should
        // span bytes 1..3 ("bc").
        e.handle_key("end", false, false, None);
        e.handle_key("left", true, false, None);
        e.handle_key("left", true, false, None);
        let (a, b) = e.selection_range();
        assert_eq!((a, b), (1, 3));
    }

    #[test]
    fn gutter_width_floors_at_four_digits() {
        let short = CodeEditor::from_string("one");
        let long = CodeEditor::from_string(&"x\n".repeat(99_999));
        // Width grows with digit count but never below the 4-digit
        // floor.
        let w_short = short.gutter_width_px();
        let w_long = long.gutter_width_px();
        assert!(w_short < w_long, "{w_short} should be < {w_long}");
        assert!(w_short >= 4.0 * 7.5 + 12.0);
    }
}
