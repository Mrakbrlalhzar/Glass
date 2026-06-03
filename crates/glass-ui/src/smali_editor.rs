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

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use gpui::Context;
use smali::types::{SmaliClass, SmaliField, SmaliMethod};

use crate::{Shell, TabKind};

/// Collect every smali class JNI in the bundle into an Arc'd set
/// suitable for stashing on a `CodeEditor`. The renderer
/// consults this to decide whether class / method / field
/// tokens get the hover-underline link affordance — references
/// to classes the bundle doesn't know about (platform types,
/// other apps) stay plain.
fn build_resolvable_classes(shell: &Shell) -> Arc<HashSet<String>> {
    let Some(bundle) = shell.bundle() else {
        return Arc::new(HashSet::new());
    };
    Arc::new(
        bundle
            .smali_classes
            .keys()
            .map(|(_, jni)| jni.clone())
            .collect(),
    )
}

/// Reorder `parsed.methods` and `parsed.fields` so the i-th
/// item matches the i-th item in `original` (keyed by name +
/// JNI signature). Members that aren't in the original move
/// to the end, preserving their relative order. Members in
/// the original that have been removed from `parsed` are
/// dropped from the result.
///
/// Why this exists: `SmaliClass::to_smali()` sorts methods +
/// fields by (name, sig) on write, so the text the user sees
/// is sorted — but the original lifted class's `methods` /
/// `fields` Vecs are in DEX byte order. Without realignment,
/// the changes-dialog's positional diff
/// (`smali_edits::diff_members`) flags every member that
/// happened to land in a different slot as "edited," even when
/// only one was actually changed.
fn align_members_to_original(original: &SmaliClass, parsed: &mut SmaliClass) {
    parsed.methods =
        align_vec(&original.methods, std::mem::take(&mut parsed.methods), |m| {
            method_key(m)
        });
    parsed.fields =
        align_vec(&original.fields, std::mem::take(&mut parsed.fields), |f| {
            field_key(f)
        });
}

fn method_key(m: &SmaliMethod) -> String {
    format!("{}{}", m.name, m.signature.to_jni())
}

fn field_key(f: &SmaliField) -> String {
    format!("{}:{}", f.name, f.signature.to_jni())
}

/// Generic alignment: produce a Vec whose i-th element is the
/// item from `from` matching original[i]'s key. Items from
/// `from` not in `original` go at the end in their existing
/// order; original items missing from `from` are dropped
/// (consistent with what a delete in the editor should look
/// like).
fn align_vec<T, F>(original: &[T], mut from: Vec<T>, key: F) -> Vec<T>
where
    F: Fn(&T) -> String,
{
    let mut out: Vec<T> = Vec::with_capacity(from.len());
    for orig in original {
        let target = key(orig);
        if let Some(pos) = from.iter().position(|item| key(item) == target) {
            out.push(from.remove(pos));
        }
    }
    // Anything left in `from` is new — append in its existing
    // (buffer) order.
    out.extend(from);
    out
}

/// How long after the last edit before the idle-reparse loop
/// will reparse a smali buffer. Short enough that link-following
/// feels responsive; long enough that mid-edit unparseable
/// states don't churn the parser.
pub(crate) const REPARSE_IDLE_MS: u64 = 500;

impl Shell {
    /// Open (or focus) a `SmaliEditor` tab for the currently-
    /// active `SmaliClass` view. Retained because some legacy
    /// code paths still reference it; new callsites should use
    /// `open_smali_editor_for_class` directly.
    pub(crate) fn open_active_smali_in_glass_editor(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active_tab else { return };
        let class_jni = match self.tabs.get(active).map(|t| &t.kind) {
            Some(TabKind::SmaliEditor { class_jni, .. }) => class_jni.clone(),
            _ => return,
        };
        self.open_smali_editor_for_class(&class_jni, cx);
    }

    /// Open (or focus) a `SmaliEditor` tab for `class_jni`. Looks
    /// up the owning artifact through the bundle's
    /// `smali_classes` map and seeds the editor from the
    /// staged-or-original class body.
    ///
    /// This is the default destination for clicking a smali
    /// class leaf in the tree (the older `SmaliClass` viewer
    /// has been retired). Safe to call when no bundle is
    /// loaded or when `class_jni` isn't a known DEX class —
    /// no-ops in those cases.
    pub(crate) fn open_smali_editor_for_class(
        &mut self,
        class_jni: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle() else { return };
        let Some((artifact, current_class)) = bundle
            .smali_classes
            .iter()
            .find_map(|((aid, jni), c)| {
                if jni == class_jni {
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
            .get(&artifact, class_jni)
            .map(|e| e.modified.clone())
            .unwrap_or(current_class);
        let body = body_class.to_smali();

        let kind = TabKind::SmaliEditor {
            artifact: artifact.clone(),
            class_jni: class_jni.to_string(),
        };
        if let Some(i) = self.tabs.iter().position(|t| t.kind == kind) {
            self.active_tab = Some(i);
            cx.notify();
            return;
        }
        let mut tab = crate::Tab::new(kind);
        let mut editor = crate::code_editor::CodeEditor::from_string(body)
            .with_highlight(crate::code_editor::HighlightMode::Smali);
        editor.reparse_smali();
        editor.resolvable_classes = build_resolvable_classes(self);
        tab.code_editor = Some(editor);
        self.tabs.push(tab);
        self.active_tab = Some(self.tabs.len() - 1);
        self.refresh_changed_rows(&artifact, class_jni);
        cx.notify();
    }

    /// Rewrite the open `SmaliEditor` buffer for
    /// `(artifact, class_jni)` to match the current staged-or-
    /// original text. Called by every revert path so the buffer
    /// stays consistent — otherwise the next auto-stage would
    /// just re-stage the in-editor (un-reverted) content.
    ///
    /// No-op when no editor is open for the class.
    pub(crate) fn resync_smali_editor_buffer(
        &mut self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
    ) {
        // Resolve the canonical body for this class — staged
        // when there's a remaining edit, else the original
        // lifted form.
        let body = {
            let Some(bundle) = self.bundle() else { return };
            let key = (artifact.clone(), class_jni.to_string());
            let Some(original) = bundle.smali_classes.get(&key) else { return };
            match bundle.smali_edits.get(artifact, class_jni) {
                Some(s) => s.modified.to_smali(),
                None => original.to_smali(),
            }
        };

        // Find the matching editor tab and replace its buffer
        // content. CodeEditor doesn't expose a "set text"
        // shortcut, but the rope-backed Buffer can replace its
        // whole content via a single full-range edit.
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
        editor.replace_all_text(&body);
        // The buffer now matches what's staged → drop dirty +
        // recompute parsed model + tint. We don't need to call
        // `auto_stage_if_changed` again; the bundle state is
        // already where it should be.
        editor.mark_clean();
        editor.reparse_smali();
        // Drop dropped-bundle borrow before refresh_changed_rows
        // takes a new one.
        self.refresh_changed_rows(artifact, class_jni);
    }

    /// Refresh the `changed_rows` set on the named SmaliEditor's
    /// CodeEditor. Diffs each member (method / field) in the
    /// buffer against its original-lifted counterpart by rendering
    /// just that member to smali and comparing line-wise. Members
    /// that aren't in the original (newly-added) count as changed.
    /// Cheap — single-class scope, line-prefix scan + per-member
    /// text render.
    pub(crate) fn refresh_changed_rows(
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
        mut parsed: smali::types::SmaliClass,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(bundle) = self.bundle() else { return false };
        let key = (artifact.clone(), class_jni.to_string());
        let Some(original) = bundle.smali_classes.get(&key) else { return false };
        // Align the parsed methods + fields with the original's
        // ordering. The smali writer sorts by (name, sig) on
        // output, so the buffer's text order may differ from
        // the original's DEX-byte order. Without this, every
        // re-ordered-on-output method looks "edited" to
        // `diff_members` (which compares positionally), and
        // one field edit balloons into hundreds of staged
        // changes in the Changes dialog.
        align_members_to_original(original, &mut parsed);
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
                let _ = bundle;
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
        let _ = bundle;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_vec_reorders_to_match_original() {
        // Strings stand in for SmaliMethod / SmaliField — the
        // key function is what align_vec uses.
        let original = vec!["alpha", "beta", "gamma"];
        // Buffer produced these in writer-sorted order (already
        // alphabetical here, but the test mimics a real
        // mismatch shape with a reordered buffer).
        let parsed = vec!["beta", "gamma", "alpha"];
        let out = align_vec(&original, parsed, |s| s.to_string());
        assert_eq!(out, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn align_vec_drops_removed_and_appends_new() {
        let original = vec!["alpha", "beta"];
        let parsed = vec!["beta", "delta"];
        let out = align_vec(&original, parsed, |s| s.to_string());
        // alpha removed (missing from parsed); beta kept in
        // position 0; delta is new, appended at the end.
        assert_eq!(out, vec!["beta", "delta"]);
    }

    #[test]
    fn align_vec_preserves_new_member_order() {
        let original = vec!["a"];
        let parsed = vec!["a", "new1", "new2"];
        let out = align_vec(&original, parsed, |s| s.to_string());
        assert_eq!(out, vec!["a", "new1", "new2"]);
    }
}
