//! Annotation-editor state-mutation methods.
//!
//! Lifted out of `shell_actions.rs` so the file stays under the
//! module-size discipline. The methods are still defined on
//! `Shell` via a sibling `impl Shell` block — Rust allows
//! multiple `impl Shell` blocks across files in the same crate,
//! so the existing call sites continue to work without renames.
//!
//! Scope: the rename/comment edit lifecycle driven through the
//! palette (`begin_annotation_edit` / `commit_annotation_edit` /
//! `cancel_annotation_edit`), the colour-picker popover (open /
//! close / pick), and the one-shot `clear_annotation_at` helper.
//! The underlying write/clear primitives (`write_annotation`,
//! `clear_annotation_full`) still live in `shell_actions.rs`.

use gpui::{Context, SharedString};

use crate::Shell;

impl Shell {
    pub(crate) fn begin_annotation_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        facet: crate::AnnotationFacet,
        current: String,
        cx: &mut Context<Self>,
    ) {
        let key_label = match &key {
            glass_db::AnnotationKey::Address(a) => format!("0x{a:x}"),
            glass_db::AnnotationKey::Symbol(s) => s.clone(),
            glass_db::AnnotationKey::Class(c) => c.clone(),
            glass_db::AnnotationKey::Method(c, m) => format!("{c}->{m}"),
            glass_db::AnnotationKey::MethodLine(c, m, line) => {
                format!("{c}->{m}#{line}")
            }
            glass_db::AnnotationKey::OpIndex {
                class_jni, method_decl, op_index,
            } => format!("{class_jni}->{method_decl}#op{op_index}"),
        };
        let chip = match facet {
            crate::AnnotationFacet::Rename => format!("Rename {key_label}"),
            crate::AnnotationFacet::Comment => format!("Comment on {key_label}"),
        };
        self.annotation_edit = Some(crate::AnnotationEdit {
            artifact,
            key,
            facet,
            chip_label: SharedString::from(chip),
        });
        self.palette_open = true;
        self.palette_query.set_text(current);
        self.palette_selected = 0;
        self.palette_list_len = 0;
        self.palette_focused = true;
        cx.notify();
    }

    /// Commit the in-progress annotation edit (called on Enter
    /// while `annotation_edit` is set). Writes through glass-api,
    /// refreshes the in-memory index, opens the pane on success.
    pub(crate) fn commit_annotation_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.annotation_edit.take() else { return };
        let value = self.palette_query.text().to_string();
        self.palette_query.clear();
        self.palette_open = false;
        self.palette_focused = false;
        let result = match edit.facet {
            crate::AnnotationFacet::Rename => {
                self.write_annotation(edit.artifact.clone(), edit.key.clone(), |a| {
                    if value.is_empty() {
                        a.rename = None;
                    } else {
                        a.rename = Some(value.clone());
                    }
                })
            }
            crate::AnnotationFacet::Comment => {
                self.write_annotation(edit.artifact.clone(), edit.key.clone(), |a| {
                    if value.is_empty() {
                        a.comment = None;
                    } else {
                        a.comment = Some(value.clone());
                    }
                })
            }
        };
        if let Err(e) = result {
            tracing::warn!("annotation edit failed: {e:#}");
        }
        cx.notify();
    }

    /// Bail out of an in-progress edit without writing.
    pub(crate) fn cancel_annotation_edit(&mut self, cx: &mut Context<Self>) {
        if self.annotation_edit.is_some() {
            self.annotation_edit = None;
            self.palette_open = false;
            self.palette_focused = false;
            self.palette_query.clear();
            cx.notify();
        }
    }

    pub(crate) fn open_colour_picker(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        current: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        // Anchor the popover near the previous context menu
        // position so it appears under the user's mouse.
        let position = self
            .context_menu
            .as_ref()
            .map(|m| m.position)
            .unwrap_or(gpui::Point {
                x: gpui::px(200.),
                y: gpui::px(200.),
            });
        self.colour_picker = Some(crate::ColourPickerState {
            artifact,
            key,
            position,
            current,
        });
        cx.notify();
    }

    pub(crate) fn close_colour_picker(&mut self, cx: &mut Context<Self>) {
        if self.colour_picker.is_some() {
            self.colour_picker = None;
            cx.notify();
        }
    }

    /// Activator for a swatch click in the colour picker. `rgba ==
    /// None` means "clear the colour facet".
    pub(crate) fn pick_colour(&mut self, rgba: Option<u32>, cx: &mut Context<Self>) {
        let Some(picker) = self.colour_picker.take() else { return };
        let result = self.write_annotation(picker.artifact, picker.key, |a| {
            a.colour = rgba;
        });
        if let Err(e) = result {
            tracing::warn!("colour pick failed: {e:#}");
        }
        cx.notify();
    }

    pub(crate) fn clear_annotation_at(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        cx: &mut Context<Self>,
    ) {
        let result = self.clear_annotation_full(artifact, key);
        if let Err(e) = result {
            tracing::warn!("clear annotation failed: {e:#}");
        }
        cx.notify();
    }
}
