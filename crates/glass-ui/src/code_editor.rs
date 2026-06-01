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
        }
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
            let line_str = SharedString::from(line_text);
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
                        .text_xs()
                        .text_color(fg)
                        .font_family(EDITOR_FONT)
                        .child(line_str),
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
