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
    /// Length in bytes of the longest line in the buffer.
    /// Drives the horizontal scrollbar's extent (`max_h_offset`)
    /// so the user can pan to the end of any line. Refreshed in
    /// `refresh_cache` after every edit.
    cached_max_line_bytes: u32,
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
    /// Most-recent save / parse-error message. Rendered in the
    /// footer; cleared on the next successful save or when the
    /// buffer changes. None = no message to surface.
    save_error: Option<String>,
    /// Window-coordinate bounds of the editor body (text area
    /// only — gutter excluded). Captured every paint via the
    /// `gpui::canvas` overlay; read at mouse-event time to
    /// translate window → buffer coords. Origin (0,0) until the
    /// first paint runs.
    pub(crate) body_bounds: gpui::Bounds<gpui::Pixels>,
    /// True between mouse-down and mouse-up inside the editor
    /// body. When set, subsequent mouse-move events extend the
    /// selection rather than just hovering.
    pub(crate) dragging: bool,
    /// Horizontal scroll offset, in pixels. The renderer
    /// shifts each row's body by `-h_offset` so long lines pan
    /// in / out of view. Updated by the scroll-wheel handler.
    pub(crate) h_offset: gpui::Pixels,
    /// Which highlighter to apply when painting line bodies.
    /// Set by the tab opener (`SmaliEditor` → Smali, plain
    /// scripts → None); the renderer branches on this.
    pub(crate) highlight: HighlightMode,
    /// Wall-clock time of the most recent edit. The idle-reparse
    /// loop reads this to decide when the buffer has settled
    /// long enough to reparse. `None` until the first edit.
    pub(crate) last_edit_at: Option<std::time::Instant>,
    /// Most-recent successful parse of the buffer text as a
    /// smali class. Used by features that need parsed state
    /// (per-row context menu, field/class link resolution).
    /// Stays at the previous good value while a new parse is
    /// failing, so the live UI doesn't flicker mid-edit.
    pub(crate) parsed_smali: Option<smali::types::SmaliClass>,
    /// Wall-clock time of the most recent reparse attempt
    /// (successful or not). Lets the idle loop skip when
    /// nothing has changed since the last attempt.
    pub(crate) last_reparse_at: Option<std::time::Instant>,
    /// Rows whose member (method or field) has been modified
    /// versus the original lifted class. Tinted in the
    /// renderer so the user can see what they've changed at a
    /// glance. Refreshed alongside `parsed_smali`.
    pub(crate) changed_rows: std::collections::HashSet<u32>,
    /// Rows whose line-prefix doesn't match any known smali
    /// directive / op shape. Tinted red so syntax errors are
    /// visible immediately — `.metho` instead of `.method` and
    /// the like. Recomputed alongside `parsed_smali`.
    pub(crate) bad_rows: std::collections::HashSet<u32>,
}

/// Identifier for a class member within a smali class. Used to
/// label buffer ranges + route revert actions to the right
/// existing Shell method.
#[derive(Clone, Debug)]
pub(crate) enum MemberId {
    /// `.method` block, identified by `name(sig)return` for use
    /// with `revert_smali_method_edit`. `method_signature_jni`
    /// is the JNI-form sig — the underlying revert keys on it.
    Method {
        name: String,
        signature_jni: String,
    },
    /// `.field` line. Same shape as methods, used with
    /// `revert_smali_field_edit`.
    Field {
        name: String,
        signature_jni: String,
    },
}

/// Per-language highlighter selection. Line-local for v1: each
/// renderer paint re-tokenises only the visible rows, so even
/// large files stay cheap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HighlightMode {
    /// No highlighting — plain text. Default for ScriptEditor
    /// tabs until we add a JS tokeniser.
    None,
    /// Smali source (DEX classes). Uses `crate::smali::tokenize_smali_line`.
    Smali,
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
        let snap = buffer.snapshot();
        let row_count = snap.row_count() as usize;
        let cached_max_line_bytes = compute_max_line_bytes(&snap);
        Self {
            buffer,
            list_state: ListState::new(row_count, ListAlignment::Top, px(2000.)),
            dirty: false,
            cached_row_count: row_count,
            cached_max_line_bytes,
            cursor: 0,
            selection_anchor: None,
            desired_column: None,
            save_error: None,
            body_bounds: gpui::Bounds::default(),
            dragging: false,
            h_offset: gpui::Pixels::from(0.),
            highlight: HighlightMode::None,
            last_edit_at: None,
            parsed_smali: None,
            last_reparse_at: None,
            changed_rows: std::collections::HashSet::new(),
            bad_rows: std::collections::HashSet::new(),
        }
    }

    /// Pick the highlighter for this editor. Returns `self` for
    /// chained construction (`CodeEditor::from_string(body).with_highlight(HighlightMode::Smali)`).
    pub fn with_highlight(mut self, mode: HighlightMode) -> Self {
        self.highlight = mode;
        self
    }

    /// Pan the horizontal scroll by `dx`, clamped to [0, max].
    /// Called from the renderer's scroll-wheel handler.
    pub fn scroll_h_by(&mut self, dx: gpui::Pixels, max: gpui::Pixels) {
        use gpui::Pixels;
        let new_offset = (self.h_offset + dx).clamp(Pixels::from(0.), max);
        self.h_offset = new_offset;
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

    /// Snapshot the buffer once and refresh the cached line count
    /// + max line width. Call after every mutation.
    fn refresh_cache(&mut self) {
        let snap = self.buffer.snapshot();
        let old_count = self.cached_row_count;
        let new_count = snap.row_count() as usize;
        self.cached_row_count = new_count;
        self.cached_max_line_bytes = compute_max_line_bytes(&snap);
        // Splice in-place to preserve scroll position. Building a
        // fresh `ListState::new` here reset `logical_scroll_top`
        // to None, which then resolves to "top of buffer" — every
        // keystroke yanked the viewport back to the top before
        // `ensure_caret_visible` ran, which then re-scrolled to
        // bring the caret into view (often at the bottom edge).
        // `splice(0..old, new)` updates the item count while
        // keeping the existing scroll offset intact.
        self.list_state.splice(0..old_count, new_count);
    }

    /// Total pixel width of the widest line — what the
    /// horizontal scrollbar can pan over. Used by the renderer.
    pub fn max_line_pixels(&self) -> f32 {
        self.cached_max_line_bytes as f32 * GLYPH_WIDTH
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
        // Any edit invalidates a stale save error — the next
        // save attempt will produce a fresh verdict.
        self.save_error = None;
        self.last_edit_at = Some(std::time::Instant::now());
        self.refresh_cache();
    }

    /// Re-parse the buffer text as a smali class and cache the
    /// result on `parsed_smali`. Driven by the idle-reparse loop
    /// in Shell once the buffer has been quiet long enough.
    /// On parse failure, keeps the previous good model so the
    /// UI doesn't flicker while the user is mid-edit.
    ///
    /// Also refreshes the line-shape error set (`bad_rows`)
    /// regardless of whether the whole-class parse succeeded —
    /// a class can be wholly parseable but still contain an
    /// unknown directive somewhere if the parser fell into a
    /// permissive arm; conversely a parse failure usually
    /// shows up as a row-shape mismatch we can locate. On
    /// success, clears `save_error`; on failure, sets it to
    /// the parser's message so the footer surfaces it.
    pub fn reparse_smali(&mut self) {
        self.last_reparse_at = Some(std::time::Instant::now());
        let body = self.text();
        self.bad_rows = compute_bad_rows(&body);
        match glass_api::parse_smali_class(&body) {
            Ok(c) => {
                self.parsed_smali = Some(c);
                self.save_error = None;
            }
            Err(e) => {
                // Hold onto the last good parse; users can keep
                // navigating links even while the syntax is
                // mid-edit and unparseable.
                self.save_error = Some(format_parse_error(&e.to_string()));
            }
        }
    }

    /// Whether an idle reparse is due — `true` if there's been
    /// an edit more recent than `last_reparse_at` *and* the
    /// most-recent edit is at least `min_idle` old. Used by the
    /// reparse loop to debounce.
    pub fn is_reparse_due(&self, min_idle: std::time::Duration) -> bool {
        let Some(edit) = self.last_edit_at else { return false };
        if let Some(last) = self.last_reparse_at {
            if last >= edit {
                return false;
            }
        }
        edit.elapsed() >= min_idle
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

    /// Move the caret by `dir * page_rows` lines. `page_rows` is
    /// supplied by the dispatcher from the renderer's body
    /// height — we don't know it here. Honours `desired_column`
    /// the same way Up/Down do.
    pub fn move_by_page(&mut self, dir: i32, page_rows: u32, shift: bool) {
        // Just repeated single-row motion — simple, and reuses
        // the existing clamp + desired-column logic.
        let steps = page_rows.max(1) as i32;
        for _ in 0..steps {
            self.move_vertical(dir, shift);
        }
    }

    /// Scroll the viewport so the caret's row is visible. Call
    /// after any motion or edit that could push the caret off
    /// screen (arrows, PageUp/Dn, Home/End, Enter, etc.).
    ///
    /// We don't use gpui's `scroll_to_reveal_item` because that
    /// snaps the viewport whenever the target row is at or
    /// above the current scroll-top — which means typing at the
    /// top of the buffer with the viewport scrolled down would
    /// yank the user back to row 0 on every keystroke. Our
    /// version is a no-op when the caret is already on a
    /// visible row.
    pub fn ensure_caret_visible(&self) {
        let row = self.cursor_point().row as usize;
        let body_h: f32 = self.body_bounds.size.height.into();
        // Fewer rows visible than the buffer claims when the
        // body is short — round down to whole rows so the last
        // partially-visible row is treated as "not really
        // visible." Min 1 so an unmeasured layout still has a
        // sensible reveal step.
        let visible_rows = ((body_h / LINE_HEIGHT).floor() as usize).max(1);
        let top = self.list_state.logical_scroll_top().item_ix;
        let bottom_exclusive = top + visible_rows;
        if row < top {
            // Above viewport: pull viewport up so caret row
            // sits at the top.
            self.list_state.scroll_to(gpui::ListOffset {
                item_ix: row,
                offset_in_item: gpui::Pixels::from(0.),
            });
        } else if row >= bottom_exclusive {
            // Below viewport: pull viewport down so caret row
            // sits at the bottom.
            let new_top = row.saturating_sub(visible_rows - 1);
            self.list_state.scroll_to(gpui::ListOffset {
                item_ix: new_top,
                offset_in_item: gpui::Pixels::from(0.),
            });
        }
        // else: already visible — no-op.
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
        // Snapshot the cursor before so we can tell whether the
        // keystroke actually moved it. We don't want to scroll
        // the viewport when a key was a no-op (e.g. backspace
        // at offset 0): the user may have trackpad-scrolled the
        // viewport away from the caret on purpose.
        let cursor_before = self.cursor;
        let result = self.handle_key_inner(key, shift, cmd, key_char);
        let moved = self.cursor != cursor_before;
        if moved || result {
            // Either the caret moved or the buffer changed — in
            // both cases the user expects to see what they're
            // doing.
            self.ensure_caret_visible();
        }
        result
    }

    fn handle_key_inner(
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

    /// Text inside the current selection. Returns `None` when
    /// there's no selection (just a caret). The Shell calls this
    /// to feed the system clipboard.
    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection_range();
        if a == b {
            return None;
        }
        let snap = self.buffer.snapshot();
        let mut s = String::with_capacity(b - a);
        for chunk in snap.as_rope().chunks_in_range(a..b) {
            s.push_str(chunk);
        }
        Some(s)
    }

    /// Cut the current selection out of the buffer. Returns the
    /// removed text so the Shell can place it on the clipboard.
    /// `None` when there's no selection — no-op.
    pub fn cut_selection(&mut self) -> Option<String> {
        let (a, b) = self.selection_range();
        if a == b {
            return None;
        }
        let s = {
            let snap = self.buffer.snapshot();
            let mut s = String::with_capacity(b - a);
            for chunk in snap.as_rope().chunks_in_range(a..b) {
                s.push_str(chunk);
            }
            s
        };
        self.apply_edit(a..b, "");
        Some(s)
    }

    /// Translate a window-coordinate point into a byte offset
    /// inside the buffer. Returns `None` when the body hasn't
    /// been laid out yet.
    ///
    /// Math: local_x — gutter — text_inset → column via
    /// `GLYPH_WIDTH`; local_y → row via `LINE_HEIGHT` plus the
    /// list's logical scroll top. Column clamps to the row's
    /// actual length so a click past the end snaps to end-of-
    /// line, and a click inside the gutter snaps to col 0 of the
    /// corresponding row.
    pub fn offset_for_window_point(
        &self,
        point: gpui::Point<gpui::Pixels>,
    ) -> Option<usize> {
        use gpui::Pixels;
        let b = self.body_bounds;
        if b.size.width <= Pixels::from(0.) || b.size.height <= Pixels::from(0.) {
            return None;
        }
        // Clamp into the bounds rather than reject outright —
        // a click 2px below the last line should still position
        // the caret at end of file; a click on the gutter snaps
        // to col 0 of the clicked row.
        let local_x: f32 = (point.x - b.origin.x)
            .clamp(Pixels::from(0.), b.size.width)
            .into();
        let local_y: f32 = (point.y - b.origin.y)
            .clamp(Pixels::from(0.), b.size.height)
            .into();

        // Subtract the gutter + text padding (`pl_2` = 8px) so
        // local_x is measured from the first character cell, then
        // add back the horizontal scroll so a click on the
        // visible line maps to the absolute column.
        let h: f32 = self.h_offset.into();
        let text_x =
            (local_x - self.gutter_width_px() - TEXT_INSET_PX).max(0.0) + h;

        // Visible-row index → buffer-row index via the list's
        // logical scroll top. ListState exposes the top item +
        // a sub-row pixel offset (the slice of the first row
        // that's been scrolled out of view); the row at the
        // top of the viewport is `top.item_ix`, and the
        // visible y=0 corresponds to buffer y = -offset_in_item
        // relative to that row's top edge.
        let top = self.list_state.logical_scroll_top();
        let offset_in_first: f32 = top.offset_in_item.into();
        let row_pixels = local_y + offset_in_first;
        let visible_row = (row_pixels / LINE_HEIGHT) as u32;
        let row = top.item_ix as u32 + visible_row;

        let snap = self.buffer.snapshot();
        let max_row = snap.max_point().row;
        let row = row.min(max_row);

        // Round to nearest glyph rather than floor — feels more
        // natural when the user clicks "between" characters.
        let col = ((text_x / GLYPH_WIDTH) + 0.5) as u32;
        let row_len = row_length_bytes(&snap, row);
        let col = col.min(row_len);

        Some(snap.point_to_offset(rope::Point::new(row, col)))
    }

    /// Move the caret to `offset`, optionally extending the
    /// selection (shift-click) or starting a fresh one. Used by
    /// click + drag handlers. Bytes outside the buffer are
    /// clamped.
    pub fn move_cursor_to_offset(&mut self, offset: usize, extend: bool) {
        self.set_cursor(offset, extend);
    }

    /// Select the word containing `offset`. A "word" is a
    /// maximal run of alphanumeric / underscore bytes —
    /// matches the convention every other code editor uses for
    /// double-click. Cursor lands at the end of the word with
    /// the selection anchor at the start.
    pub fn select_word_at(&mut self, offset: usize) {
        let snap = self.buffer.snapshot();
        let len = snap.len();
        if len == 0 {
            return;
        }
        let start = offset.min(len);
        let bytes: Vec<u8> = snap.as_rope().chunks().flat_map(|c| c.bytes()).collect();
        // Expand left.
        let mut a = start;
        while a > 0 && is_word_byte(bytes[a - 1]) {
            a -= 1;
        }
        // Expand right.
        let mut b = start;
        while b < len && is_word_byte(bytes[b]) {
            b += 1;
        }
        if a == b {
            // Cursor isn't on a word char — leave caret in place.
            return;
        }
        self.selection_anchor = Some(a);
        self.cursor = b;
        self.desired_column = None;
        self.dragging = false;
    }

    /// Begin a click-drag: place the caret + anchor at `offset`.
    /// Subsequent mouse-move events while `dragging` is true
    /// call `move_cursor_to_offset(.., true)` to extend.
    pub fn begin_click_drag(&mut self, offset: usize, extend: bool) {
        if extend {
            // Shift-click: start the selection from the existing
            // caret rather than wherever the user is clicking.
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
            self.cursor = offset.min(self.len());
            self.desired_column = None;
        } else {
            self.selection_anchor = None;
            self.cursor = offset.min(self.len());
            self.desired_column = None;
        }
        self.dragging = true;
    }

    /// End a click-drag (mouse-up).
    pub fn end_click_drag(&mut self) {
        self.dragging = false;
    }

    /// Undo the last transaction (or group of transactions when
    /// they were merged within `transaction_group_interval`).
    /// Returns true when something was undone — caller refreshes
    /// the view.
    ///
    /// `text::Buffer` records a transaction per `buffer.edit` call,
    /// and each call to `CodeEditor::apply_edit` makes exactly one,
    /// so each typed character / paste / cut becomes its own undo
    /// step. Burst-typing groups by the buffer's own interval
    /// heuristic.
    pub fn undo(&mut self) -> bool {
        if self.buffer.undo().is_some() {
            self.after_history_step();
            true
        } else {
            false
        }
    }

    /// Redo the next transaction on the redo stack. Returns
    /// true when something was redone.
    pub fn redo(&mut self) -> bool {
        if self.buffer.redo().is_some() {
            self.after_history_step();
            true
        } else {
            false
        }
    }

    /// Refresh derived state after an undo/redo. Clears
    /// selection (Zed's text::Buffer doesn't restore anchors
    /// across undo for us), clamps cursor into the new buffer
    /// length, marks the editor dirty (undo doesn't get back to
    /// "saved" — the user has to Save to clear that), and
    /// resizes the visible-row list.
    fn after_history_step(&mut self) {
        let new_len = self.buffer.snapshot().len();
        self.cursor = self.cursor.min(new_len);
        self.selection_anchor = None;
        self.desired_column = None;
        self.save_error = None;
        // Undo/redo are themselves edits from the user's POV;
        // flag dirty so the footer reflects "buffer doesn't
        // match what's on disk".
        self.dirty = true;
        self.refresh_cache();
    }

    /// Insert `text` at the caret (or replace the selection
    /// when one is active). Used by the paste flow. Returns
    /// true when the buffer changed.
    pub fn paste_text(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        self.insert_str(text);
        true
    }

    /// Clear the dirty flag (the buffer is now in sync with
    /// disk). Called by the save flow after a successful write.
    pub fn mark_clean(&mut self) {
        self.dirty = false;
        self.save_error = None;
    }

    /// Replace the entire buffer with `text`. Used by the smali
    /// editor's revert flow to push the canonical (staged-or-
    /// original) text back into the editor after a revert
    /// outside the editor — e.g. the Changes dialog's "Revert"
    /// button. Resets cursor / selection to the start and
    /// crucially does NOT bump `last_edit_at`: this isn't a
    /// user edit and we don't want the auto-stage loop to
    /// pick it up.
    pub fn replace_all_text(&mut self, text: &str) {
        let snap_len = self.buffer.snapshot().len();
        self.buffer.edit([(0..snap_len, text)]);
        self.cursor = 0;
        self.selection_anchor = None;
        self.desired_column = None;
        self.save_error = None;
        // `last_edit_at` deliberately untouched — see doc above.
        // Updating cached_max_line_bytes + cached_row_count.
        let snap = self.buffer.snapshot();
        self.cached_row_count = snap.row_count() as usize;
        self.cached_max_line_bytes = compute_max_line_bytes(&snap);
        self.list_state = ListState::new(
            self.cached_row_count,
            ListAlignment::Top,
            px(2000.),
        );
    }

    /// Surface a save / parse error to the user. Cleared on the
    /// next edit or the next successful save.
    pub fn set_save_error(&mut self, msg: impl Into<String>) {
        self.save_error = Some(msg.into());
    }

    /// Current save error, if any.
    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
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
        // GLYPH_WIDTH per digit (matches the body font) + 12px inset.
        n_digits * GLYPH_WIDTH + 12.0
    }
}

/// Return the `MemberId` covering buffer row `row`, if any.
/// Uses the same line-prefix scan as `compute_changed_rows`.
/// Returns `None` when the row is outside any `.method` / `.field`
/// declaration (e.g. class-level header, blank lines between
/// members).
pub(crate) fn member_at_row(
    buffer_text: &str,
    row: u32,
) -> Option<MemberId> {
    member_at_row_with_offset(buffer_text, row).map(|(m, _)| m)
}

/// Like [`member_at_row`] but also returns the row offset
/// within the member (0 = the `.method` / `.field` declaration
/// itself, 1 = first body line, etc.). Used by the editor's
/// context-menu builder to translate buffer rows to per-op
/// annotation keys.
pub(crate) fn member_at_row_with_offset(
    buffer_text: &str,
    row: u32,
) -> Option<(MemberId, u32)> {
    let row_usize = row as usize;
    let lines: Vec<&str> = buffer_text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if let Some(rest) = trimmed.strip_prefix(".method ") {
            let block_start = i;
            let mut end = i + 1;
            while end < lines.len() && lines[end].trim_start() != ".end method" {
                end += 1;
            }
            let block_end = end.min(lines.len().saturating_sub(1));
            if row_usize >= block_start && row_usize <= block_end {
                let key = method_key_from_decl(rest)?;
                let (name, sig) = split_method_key(&key)?;
                let offset = (row_usize - block_start) as u32;
                return Some((
                    MemberId::Method {
                        name,
                        signature_jni: sig,
                    },
                    offset,
                ));
            }
            i = block_end + 1;
            continue;
        }
        if trimmed.starts_with(".field ") {
            if i == row_usize {
                let rest = trimmed.strip_prefix(".field ")?;
                let key = field_key_from_decl(rest)?;
                let (name, sig) = key.split_once(':')?;
                return Some((
                    MemberId::Field {
                        name: name.to_string(),
                        signature_jni: sig.to_string(),
                    },
                    0,
                ));
            }
            i += 1;
            continue;
        }
        i += 1;
    }
    None
}

/// Split a method key `foo(args)ret` into `(name, "(args)ret")`.
/// The JNI-form signature is everything from the `(` onwards.
fn split_method_key(key: &str) -> Option<(String, String)> {
    let paren = key.find('(')?;
    Some((key[..paren].to_string(), key[paren..].to_string()))
}

/// Diff a buffer's text against the original lifted class and
/// return the set of buffer rows that sit inside a *changed*
/// `.method` / `.field`. A method is "changed" when its text in
/// the buffer differs (after trimming) from the original's
/// rendered form; a method that doesn't exist in the original
/// (newly-added) counts as changed wholesale.
///
/// The scan is line-prefix based — no full parse — so it stays
/// useful mid-edit when the buffer can't be parsed. Robust to
/// reordered methods (we match by `name(sig)return`, not
/// position).
pub(crate) fn compute_changed_rows(
    buffer_text: &str,
    original: &smali::types::SmaliClass,
) -> std::collections::HashSet<u32> {
    use std::collections::HashMap;
    // Index original members by their text key. Methods key on
    // `name(sig)return`; fields on `name:sig`.
    let mut orig_methods: HashMap<String, String> = HashMap::new();
    for m in &original.methods {
        let key = format!(
            "{}{}",
            m.name,
            m.signature.to_jni(),
        );
        // Rendered method text — what the buffer should match
        // when unchanged.
        orig_methods.insert(key, format!("{m}"));
    }
    let mut orig_fields: HashMap<String, String> = HashMap::new();
    for f in &original.fields {
        let key = format!("{}:{}", f.name, f.signature.to_jni());
        orig_fields.insert(key, format!("{f}"));
    }

    let mut changed: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let lines: Vec<&str> = buffer_text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(".method ") {
            // Find the matching .end method.
            let block_start = i;
            let mut end = i + 1;
            while end < lines.len() && lines[end].trim_start() != ".end method" {
                end += 1;
            }
            let block_end = end.min(lines.len().saturating_sub(1));
            // Parse the .method declaration's name + sig from
            // its tail: ` [modifiers...] name(sig)return`.
            let key = method_key_from_decl(rest);
            let buf_text = lines[block_start..=block_end].join("\n");
            let differs = match key.as_ref().and_then(|k| orig_methods.get(k)) {
                Some(orig_text) => {
                    normalise_member(orig_text) != normalise_member(&buf_text)
                }
                None => true, // new method or unrecognised decl → changed
            };
            if differs {
                for row in block_start..=block_end {
                    changed.insert(row as u32);
                }
            }
            i = block_end + 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(".field ") {
            let key = field_key_from_decl(rest);
            let differs = match key.as_ref().and_then(|k| orig_fields.get(k)) {
                Some(orig_text) => {
                    normalise_member(orig_text) != normalise_member(line)
                }
                None => true,
            };
            if differs {
                changed.insert(i as u32);
            }
            i += 1;
            continue;
        }
        i += 1;
    }
    changed
}

/// Extract the method's `name + signature_jni` from a
/// `.method` declaration's tail (the part after `.method `).
/// e.g. for `.method public foo(I)V`, returns `Some("foo(I)V")`.
/// Returns None when the line doesn't end with `name(args)ret`.
fn method_key_from_decl(rest: &str) -> Option<String> {
    // Strip modifiers (all the words before the one containing `(`).
    let tail = rest.split_whitespace().last()?;
    // Sanity: must contain `(` and `)`.
    if !tail.contains('(') || !tail.contains(')') {
        return None;
    }
    Some(tail.to_string())
}

/// Extract `name:signature_jni` from a `.field` declaration.
/// e.g. for `.field public static count:I`, returns
/// `Some("count:I")`. Handles trailing `= …` initialisers — we
/// search for the first whitespace-separated token containing
/// `:` rather than just taking the last word, which would
/// otherwise pick up the literal value.
fn field_key_from_decl(rest: &str) -> Option<String> {
    let key = rest
        .split_whitespace()
        .find(|tok| tok.contains(':'))?
        .split('=')
        .next()?
        .trim();
    if key.is_empty() {
        return None;
    }
    Some(key.to_string())
}

/// Normalise smali member text for diff comparison. Trim
/// trailing whitespace from each line and drop trailing blank
/// lines so insignificant whitespace differences don't register
/// as changes.
/// Set of buffer rows whose line-prefix doesn't look like
/// valid smali. Catches surface-level structural typos like
/// `.metho` instead of `.method` — the kinds of single-line
/// errors that would otherwise be invisible because the
/// whole-class parser fails with a single error rather than
/// pointing at the bad line.
///
/// Lines counted as valid:
///   * Empty / whitespace-only.
///   * Comments (`#…`).
///   * Known `.directives` (allowlist below).
///   * Lines inside a `.method` body (i.e. between `.method`
///     and `.end method`). We don't try to validate ops here —
///     too many shapes, and the parser does it better.
///   * Lines inside an `.annotation` block.
///
/// Anything else is flagged.
pub(crate) fn compute_bad_rows(buffer_text: &str) -> std::collections::HashSet<u32> {
    let mut bad: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut in_method = false;
    let mut in_annotation = false;
    let mut in_array_data = false;
    for (i, raw) in buffer_text.lines().enumerate() {
        let row = i as u32;
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Track context transitions first — these always count
        // as valid regardless of what comes next.
        if trimmed.starts_with(".method ")
            || trimmed.starts_with(".method\t")
            || trimmed == ".method"
        {
            in_method = true;
            continue;
        }
        if trimmed.starts_with(".end method") {
            in_method = false;
            continue;
        }
        if trimmed.starts_with(".annotation") || trimmed.starts_with(".subannotation") {
            in_annotation = true;
            continue;
        }
        if trimmed.starts_with(".end annotation")
            || trimmed.starts_with(".end subannotation")
        {
            in_annotation = false;
            continue;
        }
        if trimmed.starts_with(".array-data")
            || trimmed.starts_with(".packed-switch")
            || trimmed.starts_with(".sparse-switch")
        {
            in_array_data = true;
            continue;
        }
        if trimmed.starts_with(".end array-data")
            || trimmed.starts_with(".end packed-switch")
            || trimmed.starts_with(".end sparse-switch")
        {
            in_array_data = false;
            continue;
        }
        // Inside a method body we don't validate op shapes —
        // there are too many and the parser handles them.
        if in_method || in_annotation || in_array_data {
            continue;
        }
        // Outside any block: must be a directive we know.
        if !is_known_top_level_directive(trimmed) {
            bad.insert(row);
        }
    }
    bad
}

/// Whether `line` starts with a directive valid at the class
/// or class-annotation level. Conservative — we'd rather miss
/// a typo than flag a real directive as bad. Add new
/// directives here as the smali grammar grows.
fn is_known_top_level_directive(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        ".class",
        ".super",
        ".source",
        ".implements",
        ".field",
        ".enum",
        ".annotation",
        ".subannotation",
        ".end",
    ];
    PREFIXES.iter().any(|p| {
        line.starts_with(p)
            && line[p.len()..]
                .chars()
                .next()
                .map(|c| c.is_whitespace())
                .unwrap_or(line.len() == p.len())
    })
}

/// Strip the noisy `parsing smali body:` prefix from the
/// editor's error message. The underlying nom-error format
/// changes between smali crate versions; trying to extract a
/// specific snippet is fragile. Just expose the rest verbatim —
/// the footer is wide enough to render it and the user can copy
/// it if needed.
fn format_parse_error(raw: &str) -> String {
    let trimmed = raw.strip_prefix("parsing smali body: ").unwrap_or(raw);
    // Cap to a sensible length — gigantic nom debug strings
    // would otherwise blow out the footer.
    const MAX: usize = 220;
    if trimmed.len() > MAX {
        format!("{}…", &trimmed[..MAX.saturating_sub(1)])
    } else {
        trimmed.to_string()
    }
}

fn normalise_member(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut lines: Vec<&str> = text.lines().map(|l| l.trim_end()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

/// Walk every row of the buffer and return the longest line's
/// length in bytes. Used to size the horizontal scrollbar.
///
/// Cost: O(n_rows) point-to-offset lookups. Fine for the size
/// of files we expect; if profiling ever shows it as a hot
/// spot we can stream the rope instead.
fn compute_max_line_bytes(snap: &text::BufferSnapshot) -> u32 {
    let n_rows = snap.row_count();
    let mut max = 0u32;
    for row in 0..n_rows {
        let len = row_length_bytes(snap, row);
        if len > max {
            max = len;
        }
    }
    max
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

/// Byte counts as part of a "word" for double-click selection.
/// Matches the convention used by most code editors: letters,
/// digits, underscore. Non-ASCII bytes (UTF-8 continuation +
/// leading bytes for chars ≥ U+0080) are included so identifiers
/// in non-Latin scripts still select cleanly; punctuation /
/// whitespace / control bytes are excluded.
fn is_word_byte(b: u8) -> bool {
    if b >= 0x80 {
        return true;
    }
    b.is_ascii_alphanumeric() || b == b'_'
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
    let h_offset = editor.h_offset;
    let max_line_pixels = editor.max_line_pixels();
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
    let highlight = editor.highlight;
    // Soft "changed" row tint — committed-bg colour at low
    // alpha so the syntax-highlighted text on top stays
    // readable. Cloned into the row closure as a plain Rgba.
    let mut changed_tint = theme.state.committed_bg.rgba();
    changed_tint.a = 0.18;
    let changed_rows = std::sync::Arc::new(editor.changed_rows.clone());
    // "Bad row" tint — same translucent treatment but in the
    // errors-highlight colour. Wins over the changed tint when
    // both apply, since a parse error is the more pressing
    // signal.
    let mut bad_tint = theme.errors.highlight.rgba();
    bad_tint.a = 0.25;
    let bad_rows = std::sync::Arc::new(editor.bad_rows.clone());

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
                highlight,
            );
            // Row tint: bad (parse error) > changed (staged) >
            // nothing. Parse errors are the more pressing
            // signal so they win when both apply.
            let is_bad = bad_rows.contains(&row);
            let is_changed = changed_rows.contains(&row);
            let mut row_div = div()
                .h(px(LINE_HEIGHT))
                .w_full()
                .flex()
                .flex_row()
                .items_center();
            if is_bad {
                row_div = row_div.bg(bad_tint);
            } else if is_changed {
                row_div = row_div.bg(changed_tint);
            }
            row_div
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
                        .text_base()
                        .text_color(dim)
                        .font_family(EDITOR_FONT)
                        .child(line_no_str),
                )
                .child(
                    // Outer clips; inner content is positioned
                    // absolutely and offset by `-h_offset` so
                    // long lines pan. min_w(0) on a flex child
                    // is what actually allows the row to be
                    // narrower than its content (flex children
                    // default to min-content-width).
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .pl_2()
                        .h_full()
                        .text_base()
                        .font_family(EDITOR_FONT)
                        .relative()
                        .overflow_hidden()
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left(-h_offset)
                                .h_full()
                                .child(body_el),
                        ),
                )
                .into_any()
        }
    });

    let scrollbar =
        crate::scrollbar::list_scrollbar(&editor.list_state, border, dim);

    // Bounds-capture canvas — fills the body region, calls back
    // into Shell with its own measured bounds so click handlers
    // can map window coords → body-local.
    let weak = cx.entity().downgrade();
    let bounds_canvas = gpui::canvas(
        {
            let weak = weak.clone();
            move |bounds, _window, cx| {
                if let Some(entity) = weak.upgrade() {
                    cx.update_entity(&entity, |shell, _cx| {
                        if let Some(editor) = shell.active_code_editor_mut() {
                            editor.body_bounds = bounds;
                        }
                    });
                }
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    // Horizontal scroll extent — the user can pan up to the
    // widest line's end. Clamped to ≥ 0 so very short files
    // don't try to scroll past 0.
    let max_h = gpui::Pixels::from(max_line_pixels.max(0.0));

    // Inner body wrapper: relative so the canvas overlay can
    // size to it, holds the click + drag + scroll-wheel
    // handlers, and wraps the virtualised list.
    let weak_md = weak.clone();
    let weak_mm = weak.clone();
    let weak_mu = weak.clone();
    let weak_sw = weak.clone();
    let weak_rc = weak.clone();
    let body_wrapper = div()
        .flex_1()
        .relative()
        .overflow_hidden()
        .child(bounds_canvas)
        .child(body.size_full())
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                if let Some(entity) = weak_md.upgrade() {
                    let pos = ev.position;
                    let extend = ev.modifiers.shift;
                    let cmd = ev.modifiers.platform || ev.modifiers.control;
                    let click_count = ev.click_count;
                    cx.update_entity(&entity, |shell, cx| {
                        shell.code_editor_mouse_down(
                            pos, extend, cmd, click_count, cx,
                        );
                    });
                }
            },
        )
        .on_mouse_down(
            gpui::MouseButton::Right,
            move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                if let Some(entity) = weak_rc.upgrade() {
                    let pos = ev.position;
                    cx.update_entity(&entity, |shell, cx| {
                        shell.code_editor_open_context_menu(pos, cx);
                    });
                }
            },
        )
        .on_mouse_move(move |ev: &gpui::MouseMoveEvent, _w, cx: &mut App| {
            if ev.pressed_button != Some(gpui::MouseButton::Left) {
                return;
            }
            if let Some(entity) = weak_mm.upgrade() {
                let pos = ev.position;
                cx.update_entity(&entity, |shell, cx| {
                    shell.code_editor_mouse_drag(pos, cx);
                });
            }
        })
        .on_mouse_up(
            gpui::MouseButton::Left,
            move |_ev, _w, cx: &mut App| {
                if let Some(entity) = weak_mu.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.code_editor_mouse_up(cx);
                    });
                }
            },
        )
        .on_scroll_wheel(move |ev: &gpui::ScrollWheelEvent, _w, cx: &mut App| {
            // Horizontal scroll only — vertical is handled by
            // the inner list. Trackpad delta is fine to forward
            // directly; mouse-wheel scroll-h is rare.
            let dx = ev.delta.pixel_delta(px(22.)).x;
            if dx == gpui::Pixels::from(0.) {
                return;
            }
            if let Some(entity) = weak_sw.upgrade() {
                cx.update_entity(&entity, |shell, cx| {
                    if let Some(editor) = shell.active_code_editor_mut() {
                        editor.scroll_h_by(-dx, max_h);
                        cx.notify();
                    }
                });
            }
        });

    let h_scrollbar = crate::scrollbar::horizontal_scrollbar_offset(
        editor.h_offset,
        max_h,
        border,
        dim,
    );

    div()
        .size_full()
        .bg(panel)
        .relative()
        .child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(body_wrapper)
                .child(h_scrollbar)
                .child({
                    // Footer chip: line count + dirty / save-state.
                    // When the editor has a save_error message
                    // (parse failure, write failure, etc.) it
                    // takes over the right-hand slot tinted with
                    // the error highlight colour so it's
                    // impossible to miss.
                    let theme = crate::theme::current();
                    let (right_text, right_colour) =
                        if let Some(err) = editor.save_error() {
                            (
                                SharedString::from(err.to_string()),
                                theme.errors.highlight.rgba(),
                            )
                        } else if editor.dirty {
                            (SharedString::from("● modified"), dim)
                        } else {
                            (SharedString::from("saved"), dim)
                        };
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
                        .child(SharedString::from(format!("{row_count} lines")))
                        .child(
                            gpui::div()
                                .text_color(right_colour)
                                .child(right_text),
                        )
                }),
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
    highlight: HighlightMode,
) -> gpui::Div {
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

    // Build a list of coloured token spans
    // (byte_start, byte_end, colour, is_link). `is_link` is set
    // for tokens that cmd-click can follow (smali MethodName
    // tokens) so the renderer can give them an underline-on-
    // hover affordance.
    let token_spans: Vec<(usize, usize, gpui::Rgba, bool)> = match highlight {
        HighlightMode::None => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![(0, line_len_bytes, fg, false)]
            }
        }
        HighlightMode::Smali => {
            let chunks = crate::smali::tokenize_smali_line(text);
            let mut spans = Vec::with_capacity(chunks.len());
            let mut at = 0usize;
            for c in chunks {
                let len = c.text.len();
                let kind_rgb = crate::palette::chunk_colour(c.kind);
                // Materialise the palette's rgb into the editor's
                // Rgba form. The palette returns a packed 0xRRGGBB
                // with no alpha; build an opaque colour.
                let colour = gpui::Rgba {
                    r: ((kind_rgb >> 16) & 0xff) as f32 / 255.0,
                    g: ((kind_rgb >> 8) & 0xff) as f32 / 255.0,
                    b: (kind_rgb & 0xff) as f32 / 255.0,
                    a: 1.0,
                };
                // Link-eligible tokens:
                //   * MethodName with a target_text (the
                //     tokeniser fills this for any `Class;->name(sig)ret`).
                //   * Type tokens that contain a class JNI
                //     (`L...;`) — primitives like `I` are not
                //     links.
                let is_link = match c.kind {
                    glass_arch_arm::ChunkKind::MethodName => {
                        c.target_text.is_some()
                    }
                    glass_arch_arm::ChunkKind::Type => {
                        crate::smali::extract_class_jni(&c.text).is_some()
                    }
                    _ => false,
                };
                spans.push((at, at + len, colour, is_link));
                at += len;
            }
            if spans.is_empty() && !text.is_empty() {
                spans.push((0, line_len_bytes, fg, false));
            }
            spans
        }
    };

    // Compose a flex row of spans. We always emit at least one
    // child so empty lines still register a row height.
    let mut row_el = gpui::div()
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .text_color(fg)
        // Allow horizontal overflow on long lines — gpui's flex
        // row wraps without this.
        .whitespace_nowrap();

    use gpui::prelude::*;
    let sel_range = row_sel.filter(|(s, e)| s < e);
    // We need each link-span to have a stable element id so
    // hover styling can attach; collisions are unlikely (row +
    // start position is unique) but using both keeps the id
    // deterministic.
    let mut link_id_counter = 0usize;
    for (mut start, end, colour, is_link) in token_spans {
        // For each token span, walk through the optional
        // selection boundaries inside it so a selection that
        // overlaps part of a token splits the token into
        // selected + unselected halves preserving the token
        // colour. The selection background sits *behind* the
        // text colour — readability stays intact.
        while start < end {
            let mut chunk_end = end;
            let mut selected = false;
            if let Some((s, e)) = sel_range {
                if start < s {
                    // We're before the selection — render up to
                    // the selection's start.
                    chunk_end = end.min(s);
                } else if start < e {
                    // We're inside the selection — render up to
                    // the selection's end.
                    chunk_end = end.min(e);
                    selected = true;
                }
                // else: past the selection, full span unselected.
            }
            let slice = safe_slice(text, start, chunk_end);
            if !slice.is_empty() {
                let mut child = gpui::div()
                    .text_color(colour)
                    .child(SharedString::from(slice.to_string()));
                if selected {
                    child = child.bg(selection_colour);
                }
                if is_link {
                    // Give every link-eligible span a unique
                    // id so the hover-underline lands; the
                    // editor body already takes the cmd-click
                    // and dispatches via `try_follow_smali_link_at`,
                    // so we don't add a per-span click handler —
                    // the row-level handler is sufficient.
                    let id = SharedString::from(format!(
                        "code-link-{row}-{link_id_counter}",
                    ));
                    link_id_counter += 1;
                    row_el = row_el.child(
                        child
                            .id(id)
                            .cursor_pointer()
                            .hover(|s| s.underline()),
                    );
                } else {
                    row_el = row_el.child(child);
                }
            }
            start = chunk_end;
        }
    }
    // Empty line + active selection that spans this row — render
    // a thin highlight strip so the user can see the line is
    // selected. (The token loop emits nothing for an empty line.)
    if text.is_empty() && sel_range.is_some() {
        row_el = row_el.child(
            gpui::div()
                .w(px(GLYPH_WIDTH))
                .h_full()
                .bg(selection_colour),
        );
    }

    // Caret — same as before, positioned absolutely inside the
    // relative wrapper.
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
/// Courier New at `text_base` (16px) renders close to monospace
/// at ~9.6px per glyph. Tuned by eye to align the caret with the
/// underlying text; if it drifts we'll measure at runtime.
const GLYPH_WIDTH: f32 = 9.6;

/// Height of a single editor line. Matches the listing view's
/// row height so disassembly, smali, and the editor all share a
/// vertical rhythm. `pub` so the Shell-side PgUp/PgDn dispatcher
/// can convert pixel heights into row counts.
pub(crate) const LINE_HEIGHT: f32 = 22.0;

/// Horizontal inset between the gutter and the first character
/// of the line body. Matches `pl_2` (gpui's 0.5rem = 8px) on
/// the body span — kept as a const so click hit-testing in
/// `offset_for_window_point` stays in sync with the renderer.
const TEXT_INSET_PX: f32 = 8.0;

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
    fn selected_text_returns_only_selection() {
        let mut e = CodeEditor::from_string("hello world");
        // No selection yet → None.
        assert_eq!(e.selected_text(), None);
        // Select chars 6..11 ("world"): cursor at end then
        // shift-home then... easier: move to 6 then shift-end.
        for _ in 0..6 {
            e.handle_key("right", false, false, None);
        }
        e.handle_key("end", true, false, None);
        assert_eq!(e.selected_text().as_deref(), Some("world"));
    }

    #[test]
    fn cut_removes_selection_and_returns_text() {
        let mut e = CodeEditor::from_string("hello world");
        for _ in 0..6 {
            e.handle_key("right", false, false, None);
        }
        e.handle_key("end", true, false, None);
        let cut = e.cut_selection();
        assert_eq!(cut.as_deref(), Some("world"));
        assert_eq!(e.text(), "hello ");
        assert_eq!(e.cursor(), 6);
        assert!(e.dirty);
    }

    #[test]
    fn cut_with_no_selection_is_noop() {
        let mut e = CodeEditor::from_string("hello");
        assert_eq!(e.cut_selection(), None);
        assert_eq!(e.text(), "hello");
        assert!(!e.dirty);
    }

    #[test]
    fn paste_inserts_at_cursor() {
        let mut e = CodeEditor::from_string("ac");
        e.handle_key("right", false, false, None);
        assert!(e.paste_text("b"));
        assert_eq!(e.text(), "abc");
        assert_eq!(e.cursor(), 2);
    }

    #[test]
    fn paste_replaces_selection() {
        let mut e = CodeEditor::from_string("alpha beta gamma");
        // Select "beta" (chars 6..10).
        for _ in 0..6 {
            e.handle_key("right", false, false, None);
        }
        for _ in 0..4 {
            e.handle_key("right", true, false, None);
        }
        assert_eq!(e.selected_text().as_deref(), Some("beta"));
        assert!(e.paste_text("PASTED"));
        assert_eq!(e.text(), "alpha PASTED gamma");
    }

    #[test]
    fn offset_for_window_point_maps_clicks() {
        use gpui::{Bounds, Pixels, Point, Size};
        let mut e = CodeEditor::from_string("alpha\nbeta\ngamma");
        // Fake a body laid out at (100, 50), 400x100 px.
        e.body_bounds = Bounds {
            origin: Point {
                x: Pixels::from(100.),
                y: Pixels::from(50.),
            },
            size: Size {
                width: Pixels::from(400.),
                height: Pixels::from(100.),
            },
        };
        let gutter = e.gutter_width_px();
        // Click in the middle of "beta" (row 1). LINE_HEIGHT=22,
        // so row 1's vertical centre is at y = 50 + 22 + 11 = 83.
        // Aim at column 2 — text_x = 2 * GLYPH_WIDTH.
        let click_x = 100.0 + gutter + TEXT_INSET_PX + 2.0 * GLYPH_WIDTH;
        let click = Point {
            x: Pixels::from(click_x),
            y: Pixels::from(83.0),
        };
        let off = e.offset_for_window_point(click).unwrap();
        let p = e.buffer.snapshot().offset_to_point(off);
        assert_eq!((p.row, p.column), (1, 2));
    }

    #[test]
    fn offset_for_window_point_clicks_past_end_snap_to_eol() {
        use gpui::{Bounds, Pixels, Point, Size};
        let mut e = CodeEditor::from_string("hi\nworld");
        e.body_bounds = Bounds {
            origin: Point {
                x: Pixels::from(0.),
                y: Pixels::from(0.),
            },
            size: Size {
                width: Pixels::from(500.),
                height: Pixels::from(100.),
            },
        };
        let gutter = e.gutter_width_px();
        // Click at row 0, but 200px past where the text ends —
        // should clamp to col 2 (end of "hi").
        let click = Point {
            x: Pixels::from(gutter + TEXT_INSET_PX + 200.0),
            y: Pixels::from(11.0),
        };
        let off = e.offset_for_window_point(click).unwrap();
        let p = e.buffer.snapshot().offset_to_point(off);
        assert_eq!((p.row, p.column), (0, 2));
    }

    #[test]
    fn offset_for_window_point_gutter_click_lands_at_col_zero() {
        use gpui::{Bounds, Pixels, Point, Size};
        let mut e = CodeEditor::from_string("hello\nworld");
        e.body_bounds = Bounds {
            origin: Point {
                x: Pixels::from(0.),
                y: Pixels::from(0.),
            },
            size: Size {
                width: Pixels::from(400.),
                height: Pixels::from(100.),
            },
        };
        // Click inside the gutter on row 1 (LINE_HEIGHT=22 → y=33).
        let click = Point {
            x: Pixels::from(3.0),
            y: Pixels::from(33.0),
        };
        let off = e.offset_for_window_point(click).unwrap();
        let p = e.buffer.snapshot().offset_to_point(off);
        assert_eq!((p.row, p.column), (1, 0));
    }

    #[test]
    fn begin_click_drag_starts_selection() {
        let mut e = CodeEditor::from_string("abcdef");
        // Click at offset 2.
        e.begin_click_drag(2, false);
        assert_eq!(e.cursor(), 2);
        assert!(e.dragging);
        // Drag to offset 5 — selection should be 2..5.
        e.move_cursor_to_offset(5, true);
        assert_eq!(e.selection_range(), (2, 5));
        e.end_click_drag();
        assert!(!e.dragging);
    }

    #[test]
    fn shift_click_extends_existing_selection() {
        let mut e = CodeEditor::from_string("abcdefgh");
        // Place caret at 3 (no selection).
        e.begin_click_drag(3, false);
        e.end_click_drag();
        // Shift-click at 6 — selection should be 3..6.
        e.begin_click_drag(6, true);
        assert_eq!(e.selection_range(), (3, 6));
    }

    #[test]
    fn undo_redo_round_trip() {
        let mut e = CodeEditor::from_string("abc");
        e.handle_key("end", false, false, None);
        e.handle_key("d", false, false, Some("d"));
        e.handle_key("e", false, false, Some("e"));
        assert_eq!(e.text(), "abcde");
        // Undo each character. text::Buffer's group_interval is
        // long enough that synchronous tests bundle bursts into
        // one transaction, but our test executes serially fast
        // and they may merge. Just keep undoing until empty of
        // those edits.
        let mut undone = false;
        while e.text() != "abc" && e.undo() {
            undone = true;
        }
        assert!(undone, "expected at least one undo to succeed");
        assert_eq!(e.text(), "abc");
        // Now redo back.
        while e.text() != "abcde" && e.redo() {}
        assert_eq!(e.text(), "abcde");
    }

    #[test]
    fn undo_with_no_history_returns_false() {
        let mut e = CodeEditor::from_string("nothing edited");
        assert!(!e.undo());
        assert_eq!(e.text(), "nothing edited");
    }

    #[test]
    fn undo_marks_dirty_clamps_cursor() {
        let mut e = CodeEditor::from_string("");
        e.handle_key("a", false, false, Some("a"));
        e.handle_key("b", false, false, Some("b"));
        assert_eq!(e.cursor(), 2);
        e.mark_clean();
        assert!(!e.dirty);
        // Undo — must reflect dirty again, cursor clamped to
        // the shorter buffer.
        e.undo();
        assert!(e.dirty);
        assert!(e.cursor() <= e.text().len());
    }

    #[test]
    fn max_line_pixels_grows_with_longest_line() {
        let e = CodeEditor::from_string("hi\nhello world\nx");
        // "hello world" is 11 bytes; width = 11 * GLYPH_WIDTH.
        assert_eq!(e.max_line_pixels(), 11.0 * GLYPH_WIDTH);
    }

    #[test]
    fn scroll_h_by_clamps() {
        let mut e = CodeEditor::from_string("");
        let max = gpui::Pixels::from(200.);
        // Past the right end clamps to max.
        e.scroll_h_by(gpui::Pixels::from(500.), max);
        let h: f32 = e.h_offset.into();
        assert_eq!(h, 200.);
        // Past the left end clamps to 0.
        e.scroll_h_by(gpui::Pixels::from(-1000.), max);
        let h: f32 = e.h_offset.into();
        assert_eq!(h, 0.);
    }

    #[test]
    fn click_hit_test_accounts_for_h_offset() {
        use gpui::{Bounds, Pixels, Point, Size};
        let mut e = CodeEditor::from_string(&"x".repeat(200));
        e.body_bounds = Bounds {
            origin: Point {
                x: Pixels::from(0.),
                y: Pixels::from(0.),
            },
            size: Size {
                width: Pixels::from(400.),
                height: Pixels::from(100.),
            },
        };
        // Pan right by 50 glyphs' worth.
        let pan = 50.0 * GLYPH_WIDTH;
        e.h_offset = Pixels::from(pan);
        // Click in the middle of the visible area — say 10
        // glyphs past the gutter+inset.
        let gutter = e.gutter_width_px();
        let click_x = gutter + TEXT_INSET_PX + 10.0 * GLYPH_WIDTH;
        let click = Point {
            x: Pixels::from(click_x),
            y: Pixels::from(11.0),
        };
        let off = e.offset_for_window_point(click).unwrap();
        let p = e.buffer.snapshot().offset_to_point(off);
        // Visible col 10 + h_offset 50 → buffer col ≈ 60.
        assert_eq!((p.row, p.column), (0, 60));
    }

    #[test]
    fn select_word_at_expands_both_ways() {
        let mut e = CodeEditor::from_string("foo bar_baz qux");
        // Click inside "bar_baz" (offset 6, the 'r').
        e.select_word_at(6);
        let (a, b) = e.selection_range();
        // "bar_baz" is bytes 4..11.
        assert_eq!((a, b), (4, 11));
    }

    #[test]
    fn select_word_at_punctuation_is_noop() {
        let mut e = CodeEditor::from_string("foo + bar");
        e.cursor = 5; // start of "bar"
        // Click on the space (offset 4).
        e.select_word_at(4);
        // Cursor unchanged; no selection started.
        assert_eq!(e.cursor(), 5);
        assert_eq!(e.selection_range(), (5, 5));
    }

    #[test]
    fn select_word_at_underscore_included() {
        let mut e = CodeEditor::from_string("hello_world rest");
        e.select_word_at(3);
        let (a, b) = e.selection_range();
        assert_eq!((a, b), (0, 11));
    }

    #[test]
    fn method_key_from_decl_extracts_tail() {
        assert_eq!(
            method_key_from_decl("public foo(I)V"),
            Some("foo(I)V".to_string()),
        );
        assert_eq!(
            method_key_from_decl("public static constructor <init>()V"),
            Some("<init>()V".to_string()),
        );
        assert_eq!(method_key_from_decl("public static"), None);
    }

    #[test]
    fn field_key_from_decl_strips_initialiser() {
        assert_eq!(
            field_key_from_decl("public static count:I"),
            Some("count:I".to_string()),
        );
        // With initialiser.
        assert_eq!(
            field_key_from_decl("public static MAX:I = 0xff"),
            Some("MAX:I".to_string()),
        );
        // No colon → not a field key.
        assert_eq!(field_key_from_decl("public static"), None);
    }

    #[test]
    fn normalise_member_trims_trailing_whitespace() {
        assert_eq!(
            normalise_member(".method foo()V  \n  return-void   \n.end method\n\n\n"),
            ".method foo()V\n  return-void\n.end method",
        );
    }

    #[test]
    fn member_at_row_finds_method_block() {
        let body = ".class Lcom/A;\n.method foo(I)V\n  return-void\n.end method\n";
        // row 0 = .class (outside)
        assert!(member_at_row(body, 0).is_none());
        // rows 1..=3 = method block
        for row in 1..=3 {
            let m = member_at_row(body, row).expect("method here");
            match m {
                MemberId::Method { name, signature_jni } => {
                    assert_eq!(name, "foo");
                    assert_eq!(signature_jni, "(I)V");
                }
                _ => panic!("expected method"),
            }
        }
        // row past end = none
        assert!(member_at_row(body, 99).is_none());
    }

    #[test]
    fn member_at_row_finds_field_line() {
        let body = ".class Lcom/A;\n.field count:I\n.method foo()V\n.end method\n";
        let f = member_at_row(body, 1).expect("field here");
        match f {
            MemberId::Field { name, signature_jni } => {
                assert_eq!(name, "count");
                assert_eq!(signature_jni, "I");
            }
            _ => panic!("expected field"),
        }
    }

    #[test]
    fn split_method_key_splits_at_paren() {
        assert_eq!(
            split_method_key("foo(I)V"),
            Some(("foo".to_string(), "(I)V".to_string())),
        );
        assert_eq!(
            split_method_key("<init>()V"),
            Some(("<init>".to_string(), "()V".to_string())),
        );
        assert_eq!(split_method_key("nope"), None);
    }

    #[test]
    fn compute_bad_rows_flags_unknown_top_level_directive() {
        let body = ".class Lcom/A;\n.metho foo()V\n.end method\n";
        let bad = compute_bad_rows(body);
        // Row 1 = `.metho ...` — unknown directive at class level.
        assert!(bad.contains(&1));
        // Row 0 (`.class`) is fine.
        assert!(!bad.contains(&0));
    }

    #[test]
    fn compute_bad_rows_ignores_method_body_lines() {
        let body = ".class Lcom/A;\n.method foo()V\n  return-void\n  garbage qux\n.end method\n";
        let bad = compute_bad_rows(body);
        // Method-body lines aren't validated — we'd false-flag
        // legit ops. Only top-level directives get scrutinised.
        assert!(bad.is_empty());
    }

    #[test]
    fn compute_bad_rows_clean_class_is_clean() {
        let body = ".class Lcom/A;\n.super Ljava/lang/Object;\n.source \"A.java\"\n.field count:I\n.method foo()V\n.end method\n";
        assert!(compute_bad_rows(body).is_empty());
    }

    #[test]
    fn format_parse_error_strips_prefix() {
        let raw = "parsing smali body: something completely different";
        assert_eq!(
            format_parse_error(raw),
            "something completely different",
        );
    }

    #[test]
    fn format_parse_error_caps_at_max_length() {
        let raw =
            format!("parsing smali body: {}", "x".repeat(500));
        let formatted = format_parse_error(&raw);
        // The ellipsis is 3 bytes but 1 char in UTF-8 — count
        // chars so the assertion reads naturally.
        assert!(formatted.chars().count() <= 220);
        assert!(formatted.ends_with('…'));
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
