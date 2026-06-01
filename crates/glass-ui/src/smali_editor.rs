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

use gpui::Context;

use crate::{Shell, TabKind};

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
        tab.code_editor = Some(
            crate::code_editor::CodeEditor::from_string(body)
                .with_highlight(crate::code_editor::HighlightMode::Smali),
        );
        self.tabs.push(tab);
        self.active_tab = Some(self.tabs.len() - 1);
        cx.notify();
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
