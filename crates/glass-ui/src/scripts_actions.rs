//! Shell actions for the Frida Scripts panel.
//!
//! Keeps the scripts-panel command surface out of `shell_actions.rs`
//! (which already pushes 1500 lines). All writes go through the
//! GUI's already-open `Database` handle on Shell + direct filesystem
//! ops, mirroring what `glass-api::scripts` does but skipping the
//! re-open dance that would deadlock against the GUI's lock.
//!
//! Phase 2 surface:
//!   * `create_new_script` — pick a unique `untitled-N` slot,
//!     write an empty file + meta row, open it in the editor.
//!   * `open_script_editor` — open or focus the `ScriptEditor`
//!     tab for `name`.
//!   * `open_script_context_menu` — Phase 2 stub; the menu items
//!     (Toggle enabled / Rename / Delete) are wired in 2f.
//!   * `set_script_enabled_for_bundle` — toggles redb + refreshes.
//!   * `save_script_body` — writes through to disk + meta + refresh.

use std::path::PathBuf;

use gpui::{Context, MouseDownEvent, Window};

use crate::Shell;

/// The `row`-th line of `text` as an owned String, or `None`
/// when `row` is past the end. Used by the editor's right-click
/// menu builder to pull `.method` / `.field` declaration lines
/// straight out of the buffer.
fn nth_line_of(text: &str, row: u32) -> Option<String> {
    text.lines().nth(row as usize).map(|s| s.to_string())
}

/// Resolved kind for a cmd-clickable / right-click-Follow-able
/// token in a smali editor row. Same shape the existing
/// SmaliClass viewer's click handlers branch on.
#[derive(Clone, Debug)]
pub(crate) enum SmaliLinkTarget {
    /// `Class;->name(sig)ret` reference — navigates to the
    /// declaration via `bundle.method_lines`.
    Method { target_text: String },
    /// `Lcom/Foo;` reference — navigates by opening the smali
    /// class leaf.
    Class { class_jni: String },
    /// `Class;->name:Sig` reference — opens the "References to
    /// field" scoped palette (no single navigation target,
    /// since field reads + writes can be anywhere).
    Field { field_ref: String },
}

/// Walk the tokens of `line_text` and return the link target
/// covering the column `col_in_row` (in bytes), if any. Method
/// links beat class links when both happen to overlap.
pub(crate) fn smali_link_target_at_col(
    line_text: &str,
    col_in_row: usize,
) -> Option<SmaliLinkTarget> {
    let tokens = crate::smali::tokenize_smali_line(line_text);
    let mut at = 0usize;
    for tok in tokens {
        let tok_len = tok.text.len();
        let end = at + tok_len;
        if col_in_row >= at && col_in_row < end {
            match tok.kind {
                glass_arch_arm::ChunkKind::MethodName => {
                    if let Some(t) = tok.target_text {
                        return Some(SmaliLinkTarget::Method {
                            target_text: t,
                        });
                    }
                }
                glass_arch_arm::ChunkKind::FieldName => {
                    if let Some(t) = tok.target_text {
                        return Some(SmaliLinkTarget::Field {
                            field_ref: t,
                        });
                    }
                }
                glass_arch_arm::ChunkKind::Type => {
                    if let Some(jni) = crate::smali::extract_class_jni(&tok.text) {
                        return Some(SmaliLinkTarget::Class {
                            class_jni: jni.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
        at = end;
    }
    None
}

impl Shell {
    /// Create a fresh `untitled-N.js` and open it in the editor.
    pub(crate) fn create_new_script(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.next_untitled_script_name() else {
            tracing::warn!("create_new_script: too many untitled slots");
            return;
        };
        let dir = crate::scripts_panel::scripts_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            tracing::warn!("create_new_script: failed to create {:?}", dir);
            return;
        }
        let path = dir.join(format!("{name}.js"));
        // Touch the file. An empty body is fine — the editor
        // opens to an empty buffer and the user types into it.
        if let Err(e) = std::fs::write(&path, "") {
            tracing::warn!(
                "create_new_script: writing {}: {e}",
                path.display(),
            );
            return;
        }
        // Stamp meta with timestamps so the row sorts sanely.
        if let Some(db) = self.db.as_ref() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let meta = glass_db::ScriptMeta {
                description: String::new(),
                tags: Vec::new(),
                created_unix: now,
                modified_unix: now,
            };
            let _ = db.save_script_meta(&name, &meta);
        }
        self.refresh_scripts(cx);
        self.open_script_editor(&name, cx);
    }

    /// Pick the next free `untitled-<N>.js` slot. Caps at 999 so
    /// we don't loop forever in a degenerate case.
    fn next_untitled_script_name(&self) -> Option<String> {
        let dir = crate::scripts_panel::scripts_dir();
        let mut existing = std::collections::HashSet::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    existing.insert(stem.to_string());
                }
            }
        }
        // Also consider meta-only rows so we don't collide with
        // an orphan that the user might still want to repair.
        if let Some(db) = self.db.as_ref() {
            for name in db.all_script_meta().into_keys() {
                existing.insert(name);
            }
        }
        for i in 1..=999 {
            let candidate = if i == 1 {
                "untitled".to_string()
            } else {
                format!("untitled-{i}")
            };
            if !existing.contains(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Open (or focus) the editor tab for `name`. Loads the
    /// on-disk `.js` body into a fresh `CodeEditor` if no tab
    /// for this script is open yet; otherwise just activates the
    /// existing one.
    pub(crate) fn open_script_editor(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        let kind = crate::TabKind::ScriptEditor { name: name.to_string() };
        // Focus the existing tab if one is open. ScriptEditor is
        // PartialEq via the derived impl on TabKind, so name-equality
        // matches.
        if let Some(i) = self.tabs.iter().position(|t| t.kind == kind) {
            self.active_tab = Some(i);
            cx.notify();
            return;
        }
        // Load the body from disk. An orphaned metadata row (file
        // missing) opens to an empty buffer; the user can type fresh
        // content and save normally.
        let dir = crate::scripts_panel::scripts_dir();
        let path = dir.join(format!("{name}.js"));
        let body = std::fs::read_to_string(&path).unwrap_or_default();

        let mut tab = crate::Tab::new(kind);
        tab.code_editor = Some(crate::code_editor::CodeEditor::from_string(body));
        self.tabs.push(tab);
        self.active_tab = Some(self.tabs.len() - 1);
        cx.notify();
    }

    /// Right-click on a script row. Builds the Toggle / Delete
    /// menu and opens it anchored at the click position. Rename
    /// is intentionally absent for now — wiring it cleanly across
    /// the .js file + the metadata row + any open editor tab is
    /// a phase of its own.
    pub(crate) fn open_script_context_menu(
        &mut self,
        name: &str,
        ev: &MouseDownEvent,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let currently_enabled = self
            .scripts_panel
            .rows
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.enabled_for_bundle)
            .unwrap_or(false);
        let bundle_loaded = self
            .bundle()
            .and_then(|b| b.bundle_id.clone())
            .is_some();
        let label = gpui::SharedString::from(name.to_string());

        let mut items = Vec::new();
        // Toggle is only meaningful when a bundle is loaded —
        // enabled state is per-bundle.
        if bundle_loaded {
            items.push(crate::context_menu::ContextMenuItem::ToggleScriptEnabled {
                name: name.to_string(),
                currently_enabled,
                label: label.clone(),
            });
        }
        items.push(crate::context_menu::ContextMenuItem::DeleteScript {
            name: name.to_string(),
            label,
        });

        self.context_menu = Some(crate::context_menu::ContextMenuState {
            position: ev.position,
            items,
        });
        cx.notify();
    }

    /// Delete a script, plus close any tab that's editing it so
    /// the user doesn't end up looking at a zombie editor for a
    /// gone file. Called from the context menu's Delete item.
    pub(crate) fn delete_script_and_close_tab(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        // Close any open editor tabs for this script first. We
        // can't easily ask the user "really?" today; that's a UX
        // upgrade for later.
        let kind = crate::TabKind::ScriptEditor { name: name.to_string() };
        if let Some(idx) = self.tabs.iter().position(|t| t.kind == kind) {
            self.tabs.remove(idx);
            // Re-anchor active_tab after the removal.
            if let Some(active) = self.active_tab {
                if active == idx {
                    self.active_tab = if self.tabs.is_empty() {
                        None
                    } else {
                        Some(active.min(self.tabs.len().saturating_sub(1)))
                    };
                } else if active > idx {
                    self.active_tab = Some(active - 1);
                }
            }
        }
        self.delete_script(name, cx);
    }

    /// Toggle a script's enabled-for-bundle flag and refresh.
    pub(crate) fn set_script_enabled_for_bundle(
        &mut self,
        name: &str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(db) = self.db.as_ref() else { return };
        let Some(bid) = self.bundle().and_then(|b| b.bundle_id.clone()) else {
            return;
        };
        if let Err(e) = db.set_script_enabled(&bid, name, enabled) {
            tracing::warn!("set_script_enabled: {e}");
            return;
        }
        self.refresh_scripts(cx);
    }

    /// Returns true when the active tab is a code-editor kind
    /// (script or smali). Used by the global arrow / Cmd-C
    /// action handlers to short-circuit their default
    /// behaviour and route the key into the editor instead.
    pub(crate) fn active_tab_is_code_editor(&self) -> bool {
        self.active_tab
            .and_then(|i| self.tabs.get(i))
            .is_some_and(|t| {
                matches!(
                    t.kind,
                    crate::TabKind::ScriptEditor { .. }
                        | crate::TabKind::SmaliEditor { .. }
                )
            })
    }

    /// Route a named key (no modifiers) into the active code
    /// editor's `handle_key`. Used by the global `on_action`
    /// handlers for arrows so the editor sees motion keys even
    /// when something else has claimed them globally (the hex
    /// view has `left`/`right`; the palette has `up`/`down`).
    pub(crate) fn code_editor_handle_named_key(
        &mut self,
        key: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.active_code_editor_mut() else { return };
        editor.handle_key(key, false, false, None);
        cx.notify();
    }

    /// Move the editor caret by one "page" — derived from the
    /// captured body bounds (height in pixels ÷ LINE_HEIGHT)
    /// minus a row of overlap so the user keeps context across
    /// the jump. PgUp = dir -1, PgDn = +1.
    pub(crate) fn code_editor_page_scroll(
        &mut self,
        dir: i32,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.active_code_editor_mut() else { return };
        let body_h: f32 = editor.body_bounds.size.height.into();
        // ~LINE_HEIGHT per row; leave one row of overlap so the
        // user doesn't lose their place across the jump.
        let rows = ((body_h / crate::code_editor::LINE_HEIGHT).floor() as u32)
            .saturating_sub(1)
            .max(1);
        editor.move_by_page(dir, rows, false);
        // PgUp/Dn bypasses handle_key, so we have to drive the
        // caret-into-view step ourselves.
        editor.ensure_caret_visible();
        cx.notify();
    }

    /// Mutable handle to the active tab's code editor, if any.
    /// Used by the canvas-overlay bounds capture and the mouse
    /// click/drag dispatchers — saves callers from repeating
    /// the "active tab → code_editor.as_mut()" lookup.
    pub(crate) fn active_code_editor_mut(
        &mut self,
    ) -> Option<&mut crate::code_editor::CodeEditor> {
        let active = self.active_tab?;
        self.tabs.get_mut(active)?.code_editor.as_mut()
    }

    /// Left-button mouse-down inside the editor body. Move the
    /// caret (or extend the selection on shift-click) and mark
    /// the editor as in a drag — subsequent mouse-move events
    /// while the button is held will extend the selection.
    pub(crate) fn code_editor_mouse_down(
        &mut self,
        pos: gpui::Point<gpui::Pixels>,
        extend: bool,
        cmd: bool,
        click_count: usize,
        cx: &mut Context<Self>,
    ) {
        // Cmd-click on a method-name token in the smali editor
        // follows the link instead of placing the caret. Has to
        // run before the editor mutation below since it
        // navigates away (potentially opening a different tab).
        if cmd && click_count == 1 && self.try_follow_smali_link_at(pos, cx) {
            return;
        }

        let Some(editor) = self.active_code_editor_mut() else { return };
        let Some(off) = editor.offset_for_window_point(pos) else { return };
        if click_count >= 2 {
            // Double-click: select the word under the cursor.
            // Single + drag is still the prior path.
            editor.select_word_at(off);
        } else {
            editor.begin_click_drag(off, extend);
        }
        cx.notify();
    }

    /// If the active tab is a `SmaliEditor` and the click lands
    /// on a `MethodName` token whose `Class;->name(sig)ret`
    /// reference resolves in the bundle, navigate to that
    /// method. Returns true when navigation fired so the caller
    /// can short-circuit the normal caret-placement path.
    fn try_follow_smali_link_at(
        &mut self,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_tab else { return false };
        // Only smali editors carry the right kind of tokens.
        if !matches!(
            self.tabs.get(active).map(|t| &t.kind),
            Some(crate::TabKind::SmaliEditor { .. })
        ) {
            return false;
        }
        // Need the byte offset + the row text. Both come from
        // the editor; bundle lookups happen after so we don't
        // double-borrow.
        let (row, col_in_row, line_text) = {
            let Some(editor) = self
                .tabs
                .get(active)
                .and_then(|t| t.code_editor.as_ref())
            else {
                return false;
            };
            let Some(off) = editor.offset_for_window_point(pos) else { return false };
            let snap = editor.buffer.snapshot();
            let pt = snap.offset_to_point(off);
            let row = pt.row;
            let line_start = snap.point_to_offset(rope::Point::new(row, 0));
            let line_end = if row == snap.max_point().row {
                snap.len()
            } else {
                snap.point_to_offset(rope::Point::new(row + 1, 0)) - 1
            };
            let mut text = String::with_capacity(line_end - line_start);
            for chunk in snap.as_rope().chunks_in_range(line_start..line_end) {
                text.push_str(chunk);
            }
            (row, (off - line_start) as usize, text)
        };

        // Walk tokens to find which one covers `col_in_row`,
        // and classify it.
        let _ = row;
        let target = smali_link_target_at_col(&line_text, col_in_row);
        let Some(target) = target else { return false };
        self.follow_smali_link_target(target, cx)
    }

    /// Dispatch a resolved smali-link target. Used by cmd-click
    /// and by the right-click menu's "Follow" item.
    fn follow_smali_link_target(
        &mut self,
        target: SmaliLinkTarget,
        cx: &mut Context<Self>,
    ) -> bool {
        match target {
            SmaliLinkTarget::Method { target_text } => {
                let Some(bundle) = self.bundle() else { return false };
                let Some((leaf, line_no)) =
                    bundle.resolve_method_line(&target_text)
                else {
                    return false;
                };
                self.goto_smali_method(leaf, line_no, cx);
                true
            }
            SmaliLinkTarget::Class { class_jni } => {
                let Some(bundle) = self.bundle() else { return false };
                let Some(leaf) = bundle.resolve(&glass_db::TabState::SmaliClass {
                    class_jni,
                    scroll_line: 0,
                }) else {
                    return false;
                };
                self.open_leaf(leaf, cx);
                true
            }
            SmaliLinkTarget::Field { field_ref } => {
                // Fields have no single declaration to jump to —
                // open the scoped "References to field" palette
                // instead. Same behaviour as the existing
                // `RefsToField` context-menu item.
                let label = gpui::SharedString::from(
                    field_ref
                        .rsplit("->")
                        .next()
                        .unwrap_or(&field_ref)
                        .to_string(),
                );
                self.open_refs_to_field(field_ref, label, cx);
                true
            }
        }
    }

    /// Mouse-move while the left button is held — extend the
    /// selection to the new position. No-op when the editor
    /// isn't in the drag state (e.g. the user clicked elsewhere
    /// first and just happens to be passing over).
    pub(crate) fn code_editor_mouse_drag(
        &mut self,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.active_code_editor_mut() else { return };
        if !editor.dragging {
            return;
        }
        let Some(off) = editor.offset_for_window_point(pos) else { return };
        editor.move_cursor_to_offset(off, true);
        cx.notify();
    }

    /// Mouse-up — end the drag. Selection (if any) stays put;
    /// further keystrokes extend it via shift-arrows as usual.
    pub(crate) fn code_editor_mouse_up(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.active_code_editor_mut() else { return };
        if editor.dragging {
            editor.end_click_drag();
            cx.notify();
        }
    }

    /// Right-click in the editor body — open a small Copy /
    /// Cut / Paste menu at the click position. Copy + Cut are
    /// only included when there's an active selection; Paste
    /// only when the clipboard has text. When all three would
    /// be missing (no selection + empty clipboard) the menu
    /// isn't opened at all.
    pub(crate) fn code_editor_open_context_menu(
        &mut self,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Pull editor state + tab kind up-front so we don't
        // re-borrow self across menu construction.
        let Some(active) = self.active_tab else { return };
        let tab_kind = self.tabs.get(active).map(|t| t.kind.clone());
        let (selected, click_row, buffer_text, changed_at_row) = {
            let Some(editor) = self
                .tabs
                .get(active)
                .and_then(|t| t.code_editor.as_ref())
            else {
                return;
            };
            let row = editor
                .offset_for_window_point(pos)
                .map(|off| editor.buffer.snapshot().offset_to_point(off).row);
            let changed = row
                .map(|r| editor.changed_rows.contains(&r))
                .unwrap_or(false);
            (
                editor.selected_text(),
                row,
                editor.text(),
                changed,
            )
        };
        let clipboard_has_text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .filter(|s| !s.is_empty())
            .is_some();

        let mut items = Vec::new();
        if let Some(s) = selected {
            items.push(crate::context_menu::ContextMenuItem::CopyText {
                text: s.clone(),
                label: gpui::SharedString::from("selection"),
            });
            items.push(crate::context_menu::ContextMenuItem::EditorCut);
        }
        if clipboard_has_text {
            items.push(crate::context_menu::ContextMenuItem::EditorPaste);
        }

        // Smali-specific items when the active tab is a smali
        // editor. The rich items (navigation, annotations,
        // revert) are gathered by a dedicated helper so this
        // function stays readable.
        if let Some(crate::TabKind::SmaliEditor { artifact, class_jni }) =
            tab_kind.as_ref()
        {
            // Try to find a follow-able link under the click —
            // method ref or class type. If found, the "Follow"
            // item is prepended *before* the generic copy/cut/
            // paste so it reads as the primary action for the
            // right-click.
            let follow_target = self.code_editor_link_target_at_pos(pos);
            self.code_editor_smali_extra_items(
                artifact,
                class_jni,
                click_row,
                &buffer_text,
                changed_at_row,
                follow_target,
                &mut items,
            );
        }

        if items.is_empty() {
            return;
        }
        self.context_menu = Some(crate::context_menu::ContextMenuState {
            position: pos,
            items,
        });
        cx.notify();
    }

    /// Resolve a smali link target (method ref or class type)
    /// at the given window position. Returns None when the
    /// position isn't over a follow-able token. Used by the
    /// right-click menu builder to add a "Follow" item.
    fn code_editor_link_target_at_pos(
        &self,
        pos: gpui::Point<gpui::Pixels>,
    ) -> Option<SmaliLinkTarget> {
        let active = self.active_tab?;
        let editor = self.tabs.get(active)?.code_editor.as_ref()?;
        let off = editor.offset_for_window_point(pos)?;
        let snap = editor.buffer.snapshot();
        let pt = snap.offset_to_point(off);
        let row = pt.row;
        let line_start = snap.point_to_offset(rope::Point::new(row, 0));
        let line_end = if row == snap.max_point().row {
            snap.len()
        } else {
            snap.point_to_offset(rope::Point::new(row + 1, 0)) - 1
        };
        let mut text = String::with_capacity(line_end - line_start);
        for chunk in snap.as_rope().chunks_in_range(line_start..line_end) {
            text.push_str(chunk);
        }
        smali_link_target_at_col(&text, (off - line_start) as usize)
    }

    /// Whether the given row in the buffer text is a class-
    /// declaration row (`.class`, `.super`, `.implements`,
    /// `.source`, or sitting inside the class-level annotation
    /// block before any `.field` / `.method`). Used by the
    /// editor's right-click menu to offer "Edit class header in
    /// template…" only when the row qualifies.
    fn row_is_class_decl(buffer_text: &str, row: u32) -> bool {
        let row = row as usize;
        let mut in_class_scope = true;
        for (i, raw) in buffer_text.lines().enumerate() {
            let t = raw.trim_start();
            if t.starts_with(".field ")
                || t.starts_with(".field\t")
                || t == ".field"
                || t.starts_with(".method ")
                || t.starts_with(".method\t")
                || t == ".method"
            {
                in_class_scope = false;
                if i >= row {
                    return false;
                }
            }
            if i == row {
                return in_class_scope
                    && (t.starts_with(".class")
                        || t.starts_with(".super")
                        || t.starts_with(".implements")
                        || t.starts_with(".source")
                        || t.starts_with(".annotation")
                        || t.starts_with(".end annotation")
                        || t.starts_with(".subannotation"));
            }
        }
        false
    }

    /// Smali-specific context-menu builder: appends nav /
    /// annotation / revert items to `items` based on what
    /// (method or field) sits under the right-clicked row.
    /// Mirrors what `open_smali_context_menu` and
    /// `open_field_context_menu` produce for the SmaliClass
    /// viewer so the menu surface matches.
    fn code_editor_smali_extra_items(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        click_row: Option<u32>,
        buffer_text: &str,
        changed_at_row: bool,
        follow_target: Option<SmaliLinkTarget>,
        items: &mut Vec<crate::context_menu::ContextMenuItem>,
    ) {
        use crate::context_menu::{ContextMenuItem, FollowTarget};

        // Follow items first when a clickable target sits under
        // the cursor — same convention the listing's right-click
        // menu uses ("Follow" before generic copy / refs).
        if let Some(target) = follow_target {
            if let Some(bundle) = self.bundle() {
                match target {
                    SmaliLinkTarget::Method { target_text } => {
                        if let Some((leaf, line)) =
                            bundle.resolve_method_line(&target_text)
                        {
                            let display = target_text
                                .split('(')
                                .next()
                                .unwrap_or(&target_text)
                                .to_string();
                            let label = gpui::SharedString::from(display);
                            items.push(ContextMenuItem::Follow {
                                target: FollowTarget::SmaliMethod { leaf, line },
                                label: label.clone(),
                            });
                            items.push(ContextMenuItem::FollowInNewTab {
                                target: FollowTarget::SmaliMethod { leaf, line },
                                label,
                            });
                        }
                    }
                    SmaliLinkTarget::Class { class_jni: jni } => {
                        if let Some(leaf) =
                            bundle.resolve(&glass_db::TabState::SmaliClass {
                                class_jni: jni.clone(),
                                scroll_line: 0,
                            })
                        {
                            let label = gpui::SharedString::from(
                                crate::search::jni_to_dotted(&jni),
                            );
                            items.push(ContextMenuItem::Follow {
                                target: FollowTarget::SmaliClass { leaf },
                                label: label.clone(),
                            });
                            items.push(ContextMenuItem::FollowInNewTab {
                                target: FollowTarget::SmaliClass { leaf },
                                label,
                            });
                        }
                    }
                    SmaliLinkTarget::Field { field_ref } => {
                        // Fields resolve to a scoped xref palette
                        // rather than a target tab — reuse the
                        // existing `RefsToField` menu item shape so
                        // the same dispatcher handles it.
                        let display = field_ref
                            .rsplit("->")
                            .next()
                            .unwrap_or(&field_ref)
                            .to_string();
                        items.push(ContextMenuItem::RefsToField {
                            field_ref,
                            label: gpui::SharedString::from(display),
                        });
                    }
                }
            }
        }
        // Resolve "member at this row" up-front. Class-level
        // rows (no enclosing .method / .field) get neither
        // navigation nor revert items but still see the class-
        // wide revert below.
        let member_and_offset = click_row.and_then(|r| {
            crate::code_editor::member_at_row_with_offset(buffer_text, r)
        });

        // Whether the class has any staged edit at all — drives
        // the "Revert all changes to class" item.
        let class_has_edit = self
            .bundle()
            .map(|b| b.smali_edits.get(artifact, class_jni).is_some())
            .unwrap_or(false);

        // The DEX artifact for annotation lookups — same trick
        // `open_smali_context_menu` uses: first artifact in the
        // bundle's list.
        let dex_artifact = self
            .bundle()
            .and_then(|b| b.artifact_ids.first().cloned());

        match member_and_offset {
            Some((crate::code_editor::MemberId::Method { name, signature_jni }, line_offset)) => {
                let method_decl = format!("{name}{signature_jni}");
                let method_key = format!("{class_jni}->{method_decl}");
                let label = gpui::SharedString::from(name.clone());
                items.push(ContextMenuItem::CopyText {
                    text: method_key.clone(),
                    label: label.clone(),
                });
                items.push(ContextMenuItem::ShowDexCallGraph {
                    class_jni: class_jni.to_string(),
                    method_decl: method_decl.clone(),
                    label: label.clone(),
                });
                items.push(ContextMenuItem::CallersOfMethod {
                    method_key: method_key.clone(),
                    label: label.clone(),
                });
                // Annotation items hang off the DEX artifact. We
                // translate the row offset into either the method
                // header (offset 0) or a per-op annotation key
                // via the parsed SmaliMethod — same trick the
                // existing viewer uses.
                if let Some(artifact_id) = dex_artifact.clone() {
                    let (anno_key, existing) =
                        self.resolve_method_annotation_key(
                            class_jni,
                            &method_decl,
                            &method_key,
                            line_offset,
                            &artifact_id,
                        );
                    let comment_label = if existing.comment.is_some() {
                        "Edit comment…"
                    } else {
                        "Add comment…"
                    };
                    let line_chip = if line_offset == 0 {
                        String::new()
                    } else {
                        format!(" (line {line_offset})")
                    };
                    items.push(ContextMenuItem::EditComment {
                        artifact: artifact_id.clone(),
                        key: anno_key.clone(),
                        current: existing.comment.clone().unwrap_or_default(),
                        label: gpui::SharedString::from(format!(
                            "{comment_label}{line_chip}"
                        )),
                    });
                    items.push(ContextMenuItem::PickColour {
                        artifact: artifact_id.clone(),
                        key: anno_key.clone(),
                        current: existing.colour,
                        label: gpui::SharedString::from(format!(
                            "Set colour…{line_chip}"
                        )),
                    });
                    if !existing.is_empty() {
                        items.push(ContextMenuItem::ClearAnnotation {
                            artifact: artifact_id,
                            key: anno_key,
                            label: gpui::SharedString::from(format!(
                                "Clear annotation ({name}{line_chip})"
                            )),
                        });
                    }
                }
                // Right on the `.method` header? Offer the
                // templated header editor. Reading the buffer
                // text for the header row gives the existing
                // `open_method_edit_for_line` what it needs.
                if line_offset == 0 {
                    if let Some(header_line) =
                        nth_line_of(buffer_text, click_row.unwrap_or(0))
                    {
                        items.push(ContextMenuItem::EditSmaliMethodInTemplate {
                            line: header_line,
                        });
                    }
                }
                if changed_at_row {
                    items.push(ContextMenuItem::RevertSmaliMethodEdit {
                        artifact: artifact.clone(),
                        class_jni: class_jni.to_string(),
                        method_name: name.clone(),
                        method_signature_jni: signature_jni.clone(),
                        label: gpui::SharedString::from(format!(
                            "Revert {name}{signature_jni}"
                        )),
                    });
                }
            }
            Some((crate::code_editor::MemberId::Field { name, signature_jni }, _)) => {
                let field_ref = format!("{class_jni}->{name}:{signature_jni}");
                let label = gpui::SharedString::from(name.clone());
                items.push(ContextMenuItem::CopyText {
                    text: field_ref.clone(),
                    label: label.clone(),
                });
                items.push(ContextMenuItem::RefsToField {
                    field_ref,
                    label: label.clone(),
                });
                if let Some(field_line) =
                    nth_line_of(buffer_text, click_row.unwrap_or(0))
                {
                    items.push(ContextMenuItem::EditSmaliFieldInTemplate {
                        line: field_line,
                    });
                }
                if changed_at_row {
                    items.push(ContextMenuItem::RevertSmaliFieldEdit {
                        artifact: artifact.clone(),
                        class_jni: class_jni.to_string(),
                        field_name: name.clone(),
                        field_signature_jni: signature_jni.clone(),
                        label: gpui::SharedString::from(format!(
                            "Revert field {name}"
                        )),
                    });
                }
            }
            None => {
                // Class-header territory — `.class`, `.super`,
                // `.implements`, etc. Offer the templated
                // class-decl editor when the row qualifies.
                if let Some(row) = click_row {
                    if Self::row_is_class_decl(buffer_text, row) {
                        items.push(ContextMenuItem::EditSmaliClassDeclInTemplate);
                    }
                }
            }
        }

        if class_has_edit {
            items.push(ContextMenuItem::RevertSmaliClassEdit {
                artifact: artifact.clone(),
                class_jni: class_jni.to_string(),
                label: gpui::SharedString::from("Revert all changes to class"),
            });
        }
    }

    /// Translate a (method, line offset) into the canonical
    /// `AnnotationKey` + the existing annotation for that key.
    /// Mirrors the same translation in `open_smali_context_menu`:
    /// offset 0 → `Method`; offset >0 → `OpIndex` when the
    /// parsed method can map line offset to op index, else
    /// `MethodLine` as fallback.
    fn resolve_method_annotation_key(
        &self,
        class_jni: &str,
        method_decl: &str,
        method_key: &str,
        line_offset: u32,
        artifact: &glass_db::ArtifactId,
    ) -> (glass_db::AnnotationKey, glass_db::Annotation) {
        if line_offset == 0 {
            let k = glass_db::AnnotationKey::Method(
                class_jni.to_string(),
                method_decl.to_string(),
            );
            let e = self
                .bundle()
                .and_then(|b| b.annotations.get(artifact))
                .and_then(|idx| idx.at_method(method_key))
                .cloned()
                .unwrap_or_default();
            return (k, e);
        }
        let op_index = self.bundle().and_then(|b| {
            b.smali_classes.iter().find_map(|((_aid, jni), c)| {
                if jni == class_jni {
                    c.methods.iter().find(|m| {
                        format!("{}{}", m.name, m.signature.to_jni()) == method_decl
                    })
                } else {
                    None
                }
            })
            .and_then(|m| crate::annotations::line_offset_to_op_index(m, line_offset))
        });
        match op_index {
            Some(op_index) => {
                let k = glass_db::AnnotationKey::OpIndex {
                    class_jni: class_jni.to_string(),
                    method_decl: method_decl.to_string(),
                    op_index,
                };
                let e = self
                    .bundle()
                    .and_then(|b| b.annotations.get(artifact))
                    .and_then(|idx| idx.at_op_index(method_key, op_index))
                    .cloned()
                    .unwrap_or_default();
                (k, e)
            }
            None => {
                let k = glass_db::AnnotationKey::MethodLine(
                    class_jni.to_string(),
                    method_decl.to_string(),
                    line_offset,
                );
                let e = self
                    .bundle()
                    .and_then(|b| b.annotations.get(artifact))
                    .and_then(|idx| idx.at_method_line(method_key, line_offset))
                    .cloned()
                    .unwrap_or_default();
                (k, e)
            }
        }
    }

    /// Route a key event to the code editor on the active
    /// `ScriptEditor` or `SmaliEditor` tab. Returns true when
    /// the key was consumed (so the dispatcher can stop further
    /// handlers from firing).
    pub(crate) fn code_editor_handle_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_tab else { return false };
        let Some(tab) = self.tabs.get(active) else { return false };
        // Capture which editor kind this is so we can pick the
        // right save flow for Cmd-S.
        enum EditorKind {
            Script(String),
            Smali(glass_db::ArtifactId, String),
        }
        let editor_kind = match &tab.kind {
            crate::TabKind::ScriptEditor { name } => {
                EditorKind::Script(name.clone())
            }
            crate::TabKind::SmaliEditor { artifact, class_jni } => {
                EditorKind::Smali(artifact.clone(), class_jni.clone())
            }
            _ => return false,
        };

        let k = &ev.keystroke;
        let cmd = k.modifiers.platform || k.modifiers.control;
        let shift = k.modifiers.shift;

        // Intercept Cmd-S before forwarding to the editor.
        //
        // Scripts: save to disk + refresh meta.
        // Smali: no-op — smali edits auto-stage 500ms after
        //   the user stops typing (see `reparse_idle_smali_editors`),
        //   so there's nothing to save here. We still consume
        //   the key so an accidental Cmd-S doesn't fall through
        //   to inserting "s" or whatever.
        if cmd && !shift && k.key == "s" {
            match editor_kind {
                EditorKind::Script(name) => {
                    self.save_active_script_editor(&name, cx);
                }
                EditorKind::Smali(_, _) => {}
            }
            return true;
        }

        // Cmd-Z / Cmd-Shift-Z — undo / redo via text::Buffer's
        // transaction history. Handled before the clipboard
        // chords so shift-Z doesn't get swallowed by anything.
        if cmd && k.key == "z" {
            if let Some(editor) = self
                .tabs
                .get_mut(active)
                .and_then(|t| t.code_editor.as_mut())
            {
                let changed = if shift { editor.redo() } else { editor.undo() };
                if changed {
                    cx.notify();
                }
            }
            return true;
        }

        // Cmd-C / Cmd-X / Cmd-V — system clipboard. handle_key
        // ignores these (it returned false on cmd+letter except
        // Cmd-A), so we intercept here and call the buffer's
        // copy/cut/paste primitives.
        if cmd && !shift {
            match k.key.as_str() {
                "c" => {
                    let copied = self
                        .tabs
                        .get(active)
                        .and_then(|t| t.code_editor.as_ref())
                        .and_then(|e| e.selected_text());
                    if let Some(s) = copied {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(s));
                    }
                    return true;
                }
                "x" => {
                    let cut = self
                        .tabs
                        .get_mut(active)
                        .and_then(|t| t.code_editor.as_mut())
                        .and_then(|e| e.cut_selection());
                    if let Some(s) = cut {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(s));
                        cx.notify();
                    }
                    return true;
                }
                "v" => {
                    let pasted: Option<String> = cx
                        .read_from_clipboard()
                        .and_then(|item| item.text());
                    if let Some(text) = pasted {
                        if let Some(editor) = self
                            .tabs
                            .get_mut(active)
                            .and_then(|t| t.code_editor.as_mut())
                        {
                            if editor.paste_text(&text) {
                                cx.notify();
                            }
                        }
                    }
                    return true;
                }
                _ => {}
            }
        }

        let Some(tab) = self.tabs.get_mut(active) else { return false };
        let Some(editor) = tab.code_editor.as_mut() else { return false };
        let key_char = k.key_char.as_deref();
        editor.handle_key(&k.key, shift, cmd, key_char);
        cx.notify();
        // For SmaliEditor: if the user has just returned the
        // buffer to the on-disk state (typed and immediately
        // undid, backspaced the last edit, etc.) we want to
        // drop any staged entry right now rather than waiting
        // for the 500ms idle reparse to discover it. Cheap —
        // a single text-equality check, no parse required.
        if let EditorKind::Smali(artifact, class_jni) = editor_kind {
            self.drop_stage_if_buffer_matches_original(&artifact, &class_jni, cx);
        }
        true
    }

    /// Cheap post-edit check used by the smali editor: if the
    /// buffer text now equals the original lifted class's
    /// rendered form, drop any staged edit for the class so
    /// the Changes dialog updates immediately. No parse — just
    /// a text comparison.
    fn drop_stage_if_buffer_matches_original(
        &mut self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        cx: &mut Context<Self>,
    ) {
        let (matches, has_staged) = {
            let Some(bundle) = self.bundle() else { return };
            let key = (artifact.clone(), class_jni.to_string());
            let Some(original) = bundle.smali_classes.get(&key) else { return };
            let Some(active) = self.active_tab else { return };
            let Some(editor) = self
                .tabs
                .get(active)
                .and_then(|t| t.code_editor.as_ref())
            else {
                return;
            };
            (
                editor.text() == original.to_smali(),
                bundle.smali_edits.get(artifact, class_jni).is_some(),
            )
        };
        if matches && has_staged {
            if let Some(bundle) = self.bundle_mut() {
                bundle.smali_edits.remove(artifact, class_jni);
            }
            self.refresh_changed_rows(artifact, class_jni);
            cx.notify();
        }
    }

    /// Write the active script editor's buffer to disk via
    /// `save_script_body`. Clears the dirty flag on success;
    /// leaves the buffer alone on failure (with a log entry —
    /// no toast UX yet).
    pub(crate) fn save_active_script_editor(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active_tab else { return };
        let body = match self
            .tabs
            .get(active)
            .and_then(|t| t.code_editor.as_ref())
        {
            Some(e) => e.text(),
            None => return,
        };
        match self.save_script_body(name, &body, cx) {
            Ok(path) => {
                tracing::info!(
                    script = name,
                    path = %path.display(),
                    "saved script"
                );
                // Clear dirty so the tab label loses its `*`.
                if let Some(editor) = self
                    .tabs
                    .get_mut(active)
                    .and_then(|t| t.code_editor.as_mut())
                {
                    editor.mark_clean();
                }
                cx.notify();
            }
            Err(e) => {
                tracing::warn!(script = name, error = e, "save failed");
            }
        }
    }

    /// Write `body` to the on-disk file for `name` and bump the
    /// `modified_unix` timestamp. Used by the editor's save flow.
    pub(crate) fn save_script_body(
        &mut self,
        name: &str,
        body: &str,
        cx: &mut Context<Self>,
    ) -> Result<PathBuf, String> {
        let dir = crate::scripts_panel::scripts_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("creating {}: {e}", dir.display()))?;
        let path = dir.join(format!("{name}.js"));
        std::fs::write(&path, body)
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        if let Some(db) = self.db.as_ref() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut meta = db.script_meta(name).unwrap_or_default();
            if meta.created_unix == 0 {
                meta.created_unix = now;
            }
            meta.modified_unix = now;
            let _ = db.save_script_meta(name, &meta);
        }
        self.refresh_scripts(cx);
        Ok(path)
    }

    /// Delete a script's on-disk file, metadata, and every
    /// per-bundle enabled row. Mirrors `glass-api::delete_script`
    /// but uses the GUI's open DB handle.
    pub(crate) fn delete_script(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        let dir = crate::scripts_panel::scripts_dir();
        let path = dir.join(format!("{name}.js"));
        match std::fs::remove_file(&path) {
            Ok(_) | Err(_) => {}
        }
        if let Some(db) = self.db.as_ref() {
            let _ = db.delete_script(name);
        }
        self.refresh_scripts(cx);
    }
}
