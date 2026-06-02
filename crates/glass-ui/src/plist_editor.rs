//! Plist editor — Cmd-S commits a parse-and-serialise round-trip
//! into `plist_edits`, replacing whatever was staged before.
//!
//! Mirrors smali_editor: opening a plist leaf reuses the
//! rope-backed `CodeEditor` widget with `HighlightMode::Xml`,
//! pre-populated with either the staged text (if there's a
//! prior edit) or the source plist deserialised to XML.
//!
//! Live validation happens via the editor's normal idle pass —
//! `plist_edits::validate_xml` is cheap (linear in text length)
//! so we can re-parse on every keystroke without a debounce.

use gpui::Context;

use crate::code_editor::{CodeEditor, HighlightMode};
use crate::plist_edits;
use crate::Shell;

impl Shell {
    /// Open (or focus the existing) plist editor tab for
    /// `artifact`. No-op when no bundle is loaded or the
    /// artifact isn't a known plist source.
    pub(crate) fn open_plist_editor_for_artifact(
        &mut self,
        artifact: &glass_db::ArtifactId,
        cx: &mut Context<Self>,
    ) {
        let kind = crate::TabKind::PlistEditor {
            artifact: artifact.clone(),
        };
        if let Some(i) = self.tabs.iter().position(|t| t.kind == kind) {
            self.active_tab = Some(i);
            cx.notify();
            return;
        }

        // Build the buffer body. Prefer staged text over the
        // source so reopening the tab after an edit lands on
        // the in-progress version, not the pristine plist.
        let body: Option<String> = (|| {
            let bundle = self.bundle()?;
            if let Some(edit) = bundle.plist_edits.get(artifact) {
                return Some(edit.text_xml.clone());
            }
            let (_path, bytes) = bundle.plist_sources.get(artifact)?;
            match plist_edits::load_as_xml(bytes) {
                Ok((text, _format)) => Some(text),
                Err(_) => None,
            }
        })();

        let Some(body) = body else { return };
        let mut tab = crate::Tab::new(kind);
        let mut editor = CodeEditor::from_string(body)
            .with_highlight(HighlightMode::Xml);
        // Run an initial validation pass so the editor surfaces
        // a syntax error immediately if the source plist itself
        // is malformed (rare, but worth flagging).
        if let Err(msg) =
            plist_edits::validate_xml(&editor.text())
        {
            editor.set_save_error(msg);
        }
        tab.code_editor = Some(editor);
        self.tabs.push(tab);
        self.active_tab = Some(self.tabs.len() - 1);
        cx.notify();
    }

    /// Walk every open plist editor tab; on idle, validate
    /// + auto-stage the buffer. Mirrors
    /// `reparse_idle_smali_editors`. Parse failures surface as
    /// `save_error` on the editor (renderer paints the chip),
    /// successes stage into `bundle.plist_edits` and mark the
    /// editor clean.
    pub(crate) fn reparse_idle_plist_editors(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        use std::time::Duration;
        const IDLE_MS: u64 = 350;

        // First pass: collect (artifact, text). Two-phase to
        // dodge the simultaneous `&mut self.tabs` + `&self.bundle()`
        // borrow.
        let mut to_validate: Vec<(glass_db::ArtifactId, String)> = Vec::new();
        for tab in &mut self.tabs {
            let artifact = match &tab.kind {
                crate::TabKind::PlistEditor { artifact } => artifact.clone(),
                _ => continue,
            };
            let Some(editor) = tab.code_editor.as_mut() else { continue };
            if !editor.is_reparse_due(Duration::from_millis(IDLE_MS)) {
                continue;
            }
            to_validate.push((artifact, editor.text()));
        }
        if to_validate.is_empty() {
            return;
        }

        let mut any_change = false;
        for (artifact, text) in to_validate {
            let format = self
                .bundle()
                .and_then(|b| b.plist_sources.get(&artifact).cloned())
                .map(|(_, bytes)| plist_edits::detect_format(&bytes))
                .unwrap_or(plist_edits::PlistFormat::Xml);
            let outcome = plist_edits::serialise_to_bytes(&text, format);
            match outcome {
                Ok(bytes) => {
                    if let Some(bundle) = self.bundle_mut() {
                        bundle.plist_edits.insert(plist_edits::PlistEdit {
                            artifact: artifact.clone(),
                            source_format: format,
                            text_xml: text.clone(),
                            bytes,
                        });
                    }
                    if let Some(tab) = self
                        .tabs
                        .iter_mut()
                        .find(|t| matches!(&t.kind, crate::TabKind::PlistEditor { artifact: a } if a == &artifact))
                    {
                        if let Some(e) = tab.code_editor.as_mut() {
                            // Clear any prior parse-error chip
                            // by setting empty (renderer treats
                            // empty as no error). Don't call
                            // `mark_clean` here — the editor's
                            // dirty flag is its "buffer != on-
                            // disk source" signal, which stays
                            // dirty (we're staging an in-memory
                            // edit, not writing to disk).
                            if e.save_error()
                                .map(|m| m.starts_with("plist parse error:"))
                                .unwrap_or(false)
                            {
                                e.set_save_error(String::new());
                            }
                        }
                    }
                    any_change = true;
                }
                Err(msg) => {
                    if let Some(tab) = self
                        .tabs
                        .iter_mut()
                        .find(|t| matches!(&t.kind, crate::TabKind::PlistEditor { artifact: a } if a == &artifact))
                    {
                        if let Some(e) = tab.code_editor.as_mut() {
                            e.set_save_error(msg);
                        }
                    }
                }
            }
        }
        if any_change {
            cx.notify();
        }
    }
}
