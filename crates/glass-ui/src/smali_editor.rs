//! Shell actions for the **smali class editor** — Cmd-S on a
//! `TabKind::SmaliEditor` parses the buffer text and stages an
//! edit on the bundle. The buffer is rope-backed via the shared
//! `CodeEditor` widget.
//!
//! Why a separate module from `scripts_actions.rs`? Smali save
//! semantics are completely different: scripts write through to
//! disk (`.js` file under the user's library); smali stages into
//! the bundle's `smali_edits` map so the existing
//! `export-patched` pipeline picks it up at export time.

use std::time::Duration;

use gpui::Context;

use crate::{Shell, TabKind};

/// How long after the last edit before the idle-reparse loop
/// will reparse a smali buffer. Short enough that link-following
/// feels responsive; long enough that mid-edit unparseable
/// states don't churn the parser.
pub(crate) const REPARSE_IDLE_MS: u64 = 500;

impl Shell {
    /// Open (or focus) a `SmaliEditor` tab for the currently-
    /// active `SmaliClass` view. Used by the "Edit File" button
    /// in the toolbar to launch the in-app editor instead of an
    /// external one. No-op when the active tab isn't a smali view.
    pub(crate) fn open_active_smali_in_glass_editor(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active_tab else { return };
        let class_jni = match self.tabs.get(active).map(|t| &t.kind) {
            Some(TabKind::SmaliClass { class_jni }) => class_jni.clone(),
            _ => return,
        };
        // Find the owning artifact + the current (possibly-
        // staged) class body. Same lookup the external-editor
        // path uses.
        let Some(bundle) = self.bundle() else { return };
        let Some((artifact, current_class)) = bundle
            .smali_classes
            .iter()
            .find_map(|((aid, jni), c)| {
                if jni == &class_jni {
                    Some((aid.clone(), c.clone()))
                } else {
                    None
                }
            })
        else {
            return;
        };
        // Prefer the staged version when one exists, so the
        // editor opens with the in-progress text rather than the
        // pristine lifted source.
        let body_class = bundle
            .smali_edits
            .get(&artifact, &class_jni)
            .map(|e| e.modified.clone())
            .unwrap_or(current_class);
        let body = body_class.to_smali();

        let kind = TabKind::SmaliEditor {
            artifact: artifact.clone(),
            class_jni: class_jni.clone(),
        };
        if let Some(i) = self.tabs.iter().position(|t| t.kind == kind) {
            self.active_tab = Some(i);
            cx.notify();
            return;
        }
        let mut tab = crate::Tab::new(kind);
        let mut editor = crate::code_editor::CodeEditor::from_string(body)
            .with_highlight(crate::code_editor::HighlightMode::Smali);
        // Seed the parsed model from the initial buffer — the
        // class came in from a known-good parse upstream so this
        // should always succeed.
        editor.reparse_smali();
        tab.code_editor = Some(editor);
        self.tabs.push(tab);
        self.active_tab = Some(self.tabs.len() - 1);
        // Compute the initial changed_rows set so any pre-existing
        // staged edit (the user opened the editor on a class with
        // changes already staged from inline popovers, etc.) shows
        // tinted rows from the first paint.
        self.refresh_changed_rows(&artifact, &class_jni);
        cx.notify();
    }

    /// Refresh the `changed_rows` set on the named SmaliEditor's
    /// CodeEditor. Diffs each member (method / field) in the
    /// buffer against its original-lifted counterpart by rendering
    /// just that member to smali and comparing line-wise. Members
    /// that aren't in the original (newly-added) count as changed.
    /// Cheap — single-class scope, line-prefix scan + per-member
    /// text render.
    fn refresh_changed_rows(
        &mut self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
    ) {
        // Need the original lifted class to diff against. If the
        // bundle's gone or doesn't have it, drop any existing
        // changed set so the renderer doesn't show stale tints.
        let original = match self.bundle() {
            Some(b) => b
                .smali_classes
                .get(&(artifact.clone(), class_jni.to_string()))
                .cloned(),
            None => None,
        };
        // Find the matching tab. There can only be one per
        // (artifact, class_jni) but the loop is cheap.
        let Some(tab) = self.tabs.iter_mut().find(|t| {
            matches!(
                &t.kind,
                TabKind::SmaliEditor { artifact: a, class_jni: c }
                    if a == artifact && c == class_jni
            )
        }) else {
            return;
        };
        let Some(editor) = tab.code_editor.as_mut() else { return };
        let Some(original) = original else {
            editor.changed_rows.clear();
            return;
        };
        let buffer_text = editor.text();
        editor.changed_rows =
            crate::code_editor::compute_changed_rows(&buffer_text, &original);
    }

    /// Walk every open `SmaliEditor` tab and reparse any whose
    /// buffer has been idle for `REPARSE_IDLE_MS` since the last
    /// edit. Cheap: smali parse is microseconds; we skip tabs
    /// where the buffer hasn't been touched since the last
    /// successful reparse.
    ///
    /// When the reparse succeeds and the new class differs from
    /// the original lifted version, also stage the edit into
    /// `bundle.smali_edits` so it shows up in the Changes
    /// dialog automatically — no Cmd-S needed.
    pub(crate) fn reparse_idle_smali_editors(&mut self, cx: &mut Context<Self>) {
        // First pass: collect (artifact, class_jni, new parse).
        // Doing the parse + bundle staging in one pass would
        // require a mutable borrow of `self.tabs` and an
        // immutable borrow of `self.bundle()` concurrently, so
        // split it into two phases.
        let mut to_stage: Vec<(
            glass_db::ArtifactId,
            String,
            smali::types::SmaliClass,
        )> = Vec::new();
        for tab in &mut self.tabs {
            let (artifact, class_jni) = match &tab.kind {
                TabKind::SmaliEditor { artifact, class_jni } => {
                    (artifact.clone(), class_jni.clone())
                }
                _ => continue,
            };
            let Some(editor) = tab.code_editor.as_mut() else { continue };
            if !editor.is_reparse_due(Duration::from_millis(REPARSE_IDLE_MS)) {
                continue;
            }
            editor.reparse_smali();
            if let Some(parsed) = editor.parsed_smali.clone() {
                to_stage.push((artifact, class_jni, parsed));
            }
        }
        let mut any_staged = false;
        for (artifact, class_jni, parsed) in to_stage {
            if self.auto_stage_if_changed(&artifact, &class_jni, parsed, cx) {
                any_staged = true;
            }
        }
        if any_staged {
            cx.notify();
        }
    }

    /// Stage `parsed` against the original lifted class — but
    /// only if it actually differs from both (a) the original,
    /// and (b) anything currently staged. Returns true when a
    /// new edit landed in the changes registry.
    fn auto_stage_if_changed(
        &mut self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        parsed: smali::types::SmaliClass,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(bundle) = self.bundle() else { return false };
        let key = (artifact.clone(), class_jni.to_string());
        let Some(original) = bundle.smali_classes.get(&key) else { return false };
        // Render to smali text and compare — cheaper than a deep
        // equality on the structured classes (and matches what
        // export-patched will actually emit).
        let new_text = parsed.to_smali();
        let original_text = original.to_smali();
        if new_text == original_text {
            // No real change vs original — drop any existing
            // staged edit (the user reverted by typing back to
            // the original) and stop.
            if bundle.smali_edits.get(artifact, class_jni).is_some() {
                drop(bundle);
                if let Some(bundle) = self.bundle_mut() {
                    bundle.smali_edits.remove(artifact, class_jni);
                }
                self.refresh_changed_rows(artifact, class_jni);
                return true;
            }
            return false;
        }
        // If something's already staged and it matches the new
        // parse, skip — no-op write.
        if let Some(staged) = bundle.smali_edits.get(artifact, class_jni) {
            if staged.modified.to_smali() == new_text {
                return false;
            }
        }
        drop(bundle);
        self.stage_smali_class_edit(
            artifact.clone(),
            class_jni.to_string(),
            parsed,
            cx,
        );
        // Buffer now matches what's staged → drop the `*` in
        // the tab title. The Changes dialog is the source of
        // truth for "what's pending" from here on. Look up by
        // (artifact, class_jni) — the tab being staged might
        // not be the currently active one.
        for tab in &mut self.tabs {
            if let TabKind::SmaliEditor { artifact: a, class_jni: c } = &tab.kind {
                if a == artifact && c == class_jni {
                    if let Some(editor) = tab.code_editor.as_mut() {
                        editor.mark_clean();
                    }
                    break;
                }
            }
        }
        // Refresh the changed-rows set so tinting reflects the
        // new staged state.
        self.refresh_changed_rows(artifact, class_jni);
        true
    }

    /// Parse the active smali editor buffer and, on success,
    /// stage an edit on the bundle. On parse failure, attach the
    /// error to the editor's `save_error` so the footer
    /// surfaces it.
    pub(crate) fn save_active_smali_editor(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
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
        let parsed = match glass_api::parse_smali_class(&body) {
            Ok(c) => c,
            Err(e) => {
                if let Some(editor) = self
                    .tabs
                    .get_mut(active)
                    .and_then(|t| t.code_editor.as_mut())
                {
                    editor.set_save_error(format!("parse error: {e:#}"));
                }
                cx.notify();
                return;
            }
        };
        // Sanity check: the body's declared class must match the
        // tab's identity, else we'd stage the edit under the
        // wrong key.
        let body_jni = glass_api::smali_class_jni(&parsed);
        if body_jni != class_jni {
            if let Some(editor) = self
                .tabs
                .get_mut(active)
                .and_then(|t| t.code_editor.as_mut())
            {
                editor.set_save_error(format!(
                    "body declares class {body_jni:?} but this tab edits {class_jni:?}",
                ));
            }
            cx.notify();
            return;
        }
        self.stage_smali_class_edit(artifact, class_jni, parsed, cx);
        if let Some(editor) = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.code_editor.as_mut())
        {
            editor.mark_clean();
        }
        cx.notify();
    }
}
