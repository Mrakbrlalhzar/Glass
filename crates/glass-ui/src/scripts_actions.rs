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

    /// Route a key event to the code editor on the active
    /// `ScriptEditor` tab, when one is active. Returns true when
    /// the key was consumed (so the dispatcher can stop further
    /// handlers from firing).
    pub(crate) fn code_editor_handle_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_tab else { return false };
        let Some(tab) = self.tabs.get_mut(active) else { return false };
        if !matches!(tab.kind, crate::TabKind::ScriptEditor { .. }) {
            return false;
        }
        let Some(editor) = tab.code_editor.as_mut() else { return false };

        let k = &ev.keystroke;
        // Collapse "platform" (cmd on macOS, ctrl elsewhere) into a
        // single `cmd` flag — matches the convention used by
        // text_input.rs.
        let cmd = k.modifiers.platform || k.modifiers.control;
        let shift = k.modifiers.shift;
        let key_char = k.key_char.as_deref();
        editor.handle_key(&k.key, shift, cmd, key_char);
        cx.notify();
        true
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
