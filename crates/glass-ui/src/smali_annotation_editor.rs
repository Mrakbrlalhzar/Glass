//! Typed smali-annotation editor: Shell-side state-mutation methods.
//!
//! Lifted out of `shell_actions.rs` so the file stays under the
//! module-size discipline. Scope is the Java-style
//! `@Annotation(key = value)` typed annotations attached to smali
//! classes / fields / methods, including the sub-frame navigation
//! used to edit nested annotation values.
//!
//! Distinct from `annotation_editor.rs`, which handles
//! address-tagged user annotations (rename / comment / colour).
//!
//! The methods are still defined on `Shell` via a sibling `impl
//! Shell` block — Rust accepts multiple `impl Shell` blocks across
//! files in the same crate, so existing call sites continue to
//! work without renames.

use gpui::Context;

use crate::{Shell, TabKind};

impl Shell {
    /// Open the annotation editor against a class-level annotation.
    /// `index == None` means the user is adding a brand-new
    /// annotation (Save will push); `Some(i)` edits the existing
    /// annotation at `class.annotations[i]`.
    pub(crate) fn open_class_annotation_editor(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        // Source the existing annotation — prefer the staged class
        // so re-opens reflect prior edits.
        let frame = {
            let Some(bundle) = self.bundle() else { return };
            let class = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .or_else(|| {
                    bundle
                        .smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .cloned()
                });
            let Some(class) = class else { return };
            match index {
                Some(i) => match class.annotations.get(i) {
                    Some(a) => crate::annotation_popover::AnnotationFrame::from_annotation(
                        a, None,
                    ),
                    None => return,
                },
                None => crate::annotation_popover::AnnotationFrame::blank(None),
            }
        };
        self.annotation_stack = Some(crate::annotation_popover::AnnotationStack {
            root_target: crate::annotation_popover::AnnotationTarget::ClassAnnotation {
                artifact,
                class_jni,
                index,
            },
            frames: vec![frame],
        });
        cx.notify();
    }

    /// Open the annotation editor against a field annotation.
    pub(crate) fn open_field_annotation_editor(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        field_name: String,
        field_signature_jni: String,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let frame = {
            let Some(bundle) = self.bundle() else { return };
            let class = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .or_else(|| {
                    bundle
                        .smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .cloned()
                });
            let Some(class) = class else { return };
            let field = class.fields.iter().find(|f| {
                f.name == field_name && f.signature.to_jni() == field_signature_jni
            });
            let Some(field) = field else { return };
            match index {
                Some(i) => match field.annotations.get(i) {
                    Some(a) => crate::annotation_popover::AnnotationFrame::from_annotation(
                        a, None,
                    ),
                    None => return,
                },
                None => crate::annotation_popover::AnnotationFrame::blank(None),
            }
        };
        self.annotation_stack = Some(crate::annotation_popover::AnnotationStack {
            root_target: crate::annotation_popover::AnnotationTarget::FieldAnnotation {
                artifact,
                class_jni,
                field_name,
                field_signature_jni,
                index,
            },
            frames: vec![frame],
        });
        cx.notify();
    }

    /// Open the annotation editor against a method annotation.
    pub(crate) fn open_method_annotation_editor(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let frame = {
            let Some(bundle) = self.bundle() else { return };
            let class = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .or_else(|| {
                    bundle
                        .smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .cloned()
                });
            let Some(class) = class else { return };
            let method = class.methods.iter().find(|m| {
                m.name == method_name
                    && m.signature.to_jni() == method_signature_jni
            });
            let Some(method) = method else { return };
            match index {
                Some(i) => match method.annotations.get(i) {
                    Some(a) => crate::annotation_popover::AnnotationFrame::from_annotation(
                        a, None,
                    ),
                    None => return,
                },
                None => crate::annotation_popover::AnnotationFrame::blank(None),
            }
        };
        self.annotation_stack = Some(crate::annotation_popover::AnnotationStack {
            root_target: crate::annotation_popover::AnnotationTarget::MethodAnnotation {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                index,
            },
            frames: vec![frame],
        });
        cx.notify();
    }

    /// Push a SubAnnotation frame for `elements[elem_index]` on the
    /// top-most frame. Seeded from the snapshot already stored
    /// there; saving the child overwrites the snapshot.
    pub(crate) fn push_sub_annotation_frame(
        &mut self,
        elem_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(stack) = self.annotation_stack.as_mut() else { return };
        let Some(top) = stack.frames.last() else { return };
        let Some(elem) = top.elements.get(elem_index) else { return };
        let snapshot = match &elem.value {
            crate::annotation_popover::AnnotationValueDraft::SubAnnotation(s) => {
                (**s).clone()
            }
            _ => return,
        };
        let frame = crate::annotation_popover::AnnotationFrame::from_annotation(
            &snapshot,
            Some(elem_index),
        );
        stack.frames.push(frame);
        cx.notify();
    }

    /// Cancel the top-most annotation frame. If it's a child,
    /// returns control to its parent. If it's the root, closes the
    /// whole editor without writing anything.
    pub(crate) fn cancel_annotation_frame(&mut self, cx: &mut Context<Self>) {
        let Some(stack) = self.annotation_stack.as_mut() else { return };
        stack.frames.pop();
        if stack.frames.is_empty() {
            self.annotation_stack = None;
        }
        cx.notify();
    }

    /// Save the top-most frame.
    ///
    /// * Child frame — copy its draft back into the parent frame's
    ///   `elements[parent_element_index].value` as a fresh
    ///   `SubAnnotation` snapshot, then pop.
    /// * Root frame — write the assembled `SmaliAnnotation` through
    ///   the stack's `root_target` into the bundle's smali edits.
    pub(crate) fn commit_annotation_frame(&mut self, cx: &mut Context<Self>) {
        let Some(stack) = self.annotation_stack.as_mut() else { return };
        let Some(top) = stack.frames.last() else { return };
        if top.validate().is_err() {
            cx.notify();
            return;
        }
        if stack.frames.len() > 1 {
            // Child: copy snapshot up into parent.
            let assembled = top.to_annotation();
            let parent_idx = top.parent_element_index;
            stack.frames.pop();
            if let (Some(parent_frame), Some(elem_idx)) =
                (stack.frames.last_mut(), parent_idx)
            {
                if let Some(elem) = parent_frame.elements.get_mut(elem_idx) {
                    elem.value =
                        crate::annotation_popover::AnnotationValueDraft::SubAnnotation(
                            Box::new(assembled),
                        );
                }
            }
            cx.notify();
            return;
        }
        // Root: write into the bundle.
        let assembled = top.to_annotation();
        let target = stack.root_target.clone();
        self.annotation_stack = None;
        self.apply_annotation_root(target, assembled, cx);
    }

    /// Apply a freshly-assembled annotation back into the bundle's
    /// staged class. Splits class / field paths so each is plainly
    /// readable.
    fn apply_annotation_root(
        &mut self,
        target: crate::annotation_popover::AnnotationTarget,
        annotation: smali::types::SmaliAnnotation,
        cx: &mut Context<Self>,
    ) {
        use crate::annotation_popover::AnnotationTarget;
        match target {
            AnnotationTarget::ClassAnnotation { artifact, class_jni, index } => {
                self.write_class_annotation(artifact, class_jni, index, annotation, cx);
            }
            AnnotationTarget::FieldAnnotation {
                artifact,
                class_jni,
                field_name,
                field_signature_jni,
                index,
            } => {
                self.write_field_annotation(
                    artifact,
                    class_jni,
                    field_name,
                    field_signature_jni,
                    index,
                    annotation,
                    cx,
                );
            }
            AnnotationTarget::MethodAnnotation {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                index,
            } => {
                self.write_method_annotation(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    index,
                    annotation,
                    cx,
                );
            }
        }
    }

    fn write_class_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        index: Option<usize>,
        annotation: smali::types::SmaliAnnotation,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            match index {
                Some(i) => {
                    if i < class.annotations.len() {
                        class.annotations[i] = annotation;
                    } else {
                        class.annotations.push(annotation);
                    }
                }
                None => class.annotations.push(annotation),
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    fn write_field_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        field_name: String,
        field_signature_jni: String,
        index: Option<usize>,
        annotation: smali::types::SmaliAnnotation,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if let Some(field) = class.fields.iter_mut().find(|f| {
                f.name == field_name && f.signature.to_jni() == field_signature_jni
            }) {
                match index {
                    Some(i) => {
                        if i < field.annotations.len() {
                            field.annotations[i] = annotation;
                        } else {
                            field.annotations.push(annotation);
                        }
                    }
                    None => field.annotations.push(annotation),
                }
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    fn write_method_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        index: Option<usize>,
        annotation: smali::types::SmaliAnnotation,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if let Some(method) = class.methods.iter_mut().find(|m| {
                m.name == method_name && m.signature.to_jni() == method_signature_jni
            }) {
                match index {
                    Some(i) => {
                        if i < method.annotations.len() {
                            method.annotations[i] = annotation;
                        } else {
                            method.annotations.push(annotation);
                        }
                    }
                    None => method.annotations.push(annotation),
                }
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    /// Helper: take the staged-or-original SmaliClass for
    /// `(artifact, class_jni)`, hand it to `f` for mutation, and
    /// return the mutated copy. Returns `None` if no such class is
    /// loaded.
    fn with_staged_class<F>(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        f: F,
    ) -> Option<smali::types::SmaliClass>
    where
        F: FnOnce(&mut smali::types::SmaliClass),
    {
        let bundle = self.bundle()?;
        let mut class = bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            })?;
        f(&mut class);
        Some(class)
    }

    /// Helper: stage a modified class and invalidate any open
    /// smali tabs viewing it.
    pub(crate) fn stage_smali_class_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        modified: smali::types::SmaliClass,
        cx: &mut Context<Self>,
    ) {
        if let Some(bundle) = self.bundle_mut() {
            bundle.smali_edits.insert(crate::smali_edits::SmaliEdit {
                key: crate::smali_edits::SmaliEditKey {
                    artifact,
                    class_jni: class_jni.clone(),
                },
                modified,
            });
        }
        for tab in &mut self.tabs {
            if let TabKind::SmaliEditor { class_jni: jni, .. } = &tab.kind {
                if jni == &class_jni {
                    // Capture scroll position so we can restore the
                    // viewport after the line cache is rebuilt —
                    // otherwise every Enter on the op editor yanks
                    // the user back to the top of the file.
                    tab.pending_scroll_restore =
                        Some(tab.scroll.logical_scroll_top());
                    tab.lines = None;
                }
            }
        }
        cx.notify();
    }

    /// Remove a class-level annotation outright. Wired from the
    /// "× remove" affordance on the class-decl popover's annotation
    /// list.
    pub(crate) fn remove_class_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if index < class.annotations.len() {
                class.annotations.remove(index);
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    /// Remove a field annotation outright. Wired from the field
    /// popover's annotation list.
    pub(crate) fn remove_field_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        field_name: String,
        field_signature_jni: String,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if let Some(field) = class.fields.iter_mut().find(|f| {
                f.name == field_name && f.signature.to_jni() == field_signature_jni
            }) {
                if index < field.annotations.len() {
                    field.annotations.remove(index);
                }
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    /// Remove a method annotation outright. Wired from the method
    /// popover's annotation list.
    pub(crate) fn remove_method_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if let Some(method) = class.methods.iter_mut().find(|m| {
                m.name == method_name
                    && m.signature.to_jni() == method_signature_jni
            }) {
                if index < method.annotations.len() {
                    method.annotations.remove(index);
                }
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    /// Annotations currently attached to `(artifact, class_jni)`,
    /// preferring the staged class when one exists. Returns
    /// (vis, type_jni) summaries suitable for the popover row
    /// list. Returns empty if the class isn't loaded.
    pub(crate) fn class_annotation_summaries(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
    ) -> Vec<(String, String)> {
        let Some(bundle) = self.bundle() else { return Vec::new() };
        let class = bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            });
        let Some(class) = class else { return Vec::new() };
        class
            .annotations
            .iter()
            .map(|a| (a.visibility.to_str().to_string(), a.annotation_type.to_jni()))
            .collect()
    }

    /// Same shape as `class_annotation_summaries`, scoped to a
    /// specific field within the class.
    pub(crate) fn field_annotation_summaries(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        field_name: &str,
        field_signature_jni: &str,
    ) -> Vec<(String, String)> {
        let Some(bundle) = self.bundle() else { return Vec::new() };
        let class = bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            });
        let Some(class) = class else { return Vec::new() };
        let Some(field) = class.fields.iter().find(|f| {
            f.name == field_name && f.signature.to_jni() == field_signature_jni
        }) else {
            return Vec::new();
        };
        field
            .annotations
            .iter()
            .map(|a| (a.visibility.to_str().to_string(), a.annotation_type.to_jni()))
            .collect()
    }

    /// Same shape as `field_annotation_summaries`, scoped to a
    /// method within the class.
    pub(crate) fn method_annotation_summaries(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        method_name: &str,
        method_signature_jni: &str,
    ) -> Vec<(String, String)> {
        let Some(bundle) = self.bundle() else { return Vec::new() };
        let class = bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            });
        let Some(class) = class else { return Vec::new() };
        let Some(method) = class.methods.iter().find(|m| {
            m.name == method_name && m.signature.to_jni() == method_signature_jni
        }) else {
            return Vec::new();
        };
        method
            .annotations
            .iter()
            .map(|a| (a.visibility.to_str().to_string(), a.annotation_type.to_jni()))
            .collect()
    }
}
