//! AndroidManifest editor — mirrors `plist_editor` in shape.
//!
//! Opening a manifest leaf reuses the rope-backed `CodeEditor`
//! with `HighlightMode::Xml`, pre-populated with either the
//! staged XML text (if a prior edit exists) or the source AXML
//! decoded via `manifest_edits::load_as_xml`. Idle reparse
//! validates and stages an updated `ManifestEdit` into
//! `bundle.manifest_edits`; the export path drains the registry
//! at save time.

use gpui::Context;

use crate::code_editor::{CodeEditor, HighlightMode};
use crate::manifest_edits;
use crate::Shell;

impl Shell {
    /// Open (or focus the existing) manifest editor tab for
    /// `artifact`. No-op when no bundle is loaded or the
    /// artifact isn't a known manifest source.
    pub(crate) fn open_manifest_editor_for_artifact(
        &mut self,
        artifact: &glass_db::ArtifactId,
        cx: &mut Context<Self>,
    ) {
        let kind = crate::TabKind::ManifestEditor {
            artifact: artifact.clone(),
        };
        if let Some(i) = self.tabs.iter().position(|t| t.kind == kind) {
            self.active_tab = Some(i);
            cx.notify();
            return;
        }

        // Prefer staged text over source so reopening lands on
        // the in-progress edit.
        let body: Option<String> = (|| {
            let bundle = self.bundle()?;
            if let Some(edit) = bundle.manifest_edits.get(artifact) {
                return Some(edit.text_xml.clone());
            }
            let (_path, bytes) = bundle.manifest_sources.get(artifact)?;
            manifest_edits::load_as_xml(bytes).ok()
        })();

        let Some(body) = body else { return };
        let mut tab = crate::Tab::new(kind);
        let mut editor = CodeEditor::from_string(body)
            .with_highlight(HighlightMode::Xml);
        if let Err(msg) = manifest_edits::validate_xml(&editor.text()) {
            editor.set_save_error(msg);
        }
        tab.code_editor = Some(editor);
        self.tabs.push(tab);
        self.active_tab = Some(self.tabs.len() - 1);
        cx.notify();
    }

    /// Walk every open manifest editor tab; on idle, validate
    /// + auto-stage the buffer. Sibling of
    /// `reparse_idle_plist_editors` / `reparse_idle_smali_editors`.
    pub(crate) fn reparse_idle_manifest_editors(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        use std::time::Duration;
        const IDLE_MS: u64 = 350;

        let mut to_validate: Vec<(glass_db::ArtifactId, String)> = Vec::new();
        for tab in &mut self.tabs {
            let artifact = match &tab.kind {
                crate::TabKind::ManifestEditor { artifact } => artifact.clone(),
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
            match manifest_edits::serialise_to_bytes(&text) {
                Ok(bytes) => {
                    if let Some(bundle) = self.bundle_mut() {
                        bundle.manifest_edits.insert(manifest_edits::ManifestEdit {
                            artifact: artifact.clone(),
                            text_xml: text.clone(),
                            bytes,
                        });
                    }
                    if let Some(tab) = self.tabs.iter_mut().find(|t| {
                        matches!(&t.kind, crate::TabKind::ManifestEditor { artifact: a } if a == &artifact)
                    }) {
                        if let Some(e) = tab.code_editor.as_mut() {
                            if e.save_error()
                                .map(|m| m.starts_with("manifest parse error:"))
                                .unwrap_or(false)
                            {
                                e.set_save_error(String::new());
                            }
                        }
                    }
                    any_change = true;
                }
                Err(msg) => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| {
                        matches!(&t.kind, crate::TabKind::ManifestEditor { artifact: a } if a == &artifact)
                    }) {
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
