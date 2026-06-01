//! Per-op inline editor: Shell-side glue.
//!
//! Lifted out of `shell_actions.rs` so the file stays under the
//! module-size discipline. The state type, rendering, parsing
//! helpers and suggestion-context classifier live in
//! `crate::op_editor`; this module holds only the Shell-side
//! state-mutation methods that open / cancel / commit the editor
//! and refresh its autocomplete list.
//!
//! The methods are still defined on `Shell` via a sibling `impl
//! Shell` block — Rust accepts multiple `impl Shell` blocks across
//! files in the same crate, so the existing call sites continue to
//! work without renames.

use gpui::{Context, SharedString};

use crate::shell_actions::COMMON_EXTERNAL_TYPES;
use crate::{Shell, TabKind};

impl Shell {
    // ---- Per-op inline editor -----------------------------------------

    /// Enter-on-row entry point. Opens the per-op editor when
    /// the selected row sits inside a method body (not on the
    /// `.method` header itself — that's the method-header
    /// popover's territory). Returns `true` to short-circuit
    /// the normal Enter chain.
    pub(crate) fn smali_open_op_edit_at_selection(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_tab else { return false };
        let Some(tab) = self.tabs.get(active) else { return false };
        if !matches!(tab.kind, TabKind::SmaliEditor { .. }) {
            return false;
        }
        let Some(row) = tab.selected_row else { return false };
        self.open_op_edit_for_row(row, cx)
    }

    /// Open the inline op editor on `row_index` in the active
    /// smali tab. Returns whether it opened — `false` if the row
    /// isn't inside a method body, the method can't be resolved,
    /// or no class is loaded.
    pub(crate) fn open_op_edit_for_row(
        &mut self,
        row_index: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.op_edit.is_some() {
            // Already editing — don't stack edits.
            return false;
        }
        let Some(active) = self.active_tab else { return false };
        let Some(class_jni) = self.tabs.get(active).and_then(|t| match &t.kind {
            TabKind::SmaliEditor { class_jni, .. } => Some(class_jni.clone()),
            _ => None,
        }) else {
            return false;
        };
        let Some(tab) = self.tabs.get(active) else { return false };
        let Some(lines) = tab.lines.as_ref() else { return false };
        let Some(line_text) = lines.get(row_index).cloned() else { return false };
        // Find the enclosing `.method` row. If the user clicked
        // the header itself, defer to the method-header popover
        // by returning false.
        let mut header_row = None;
        for j in (0..=row_index).rev() {
            let Some(l) = lines.get(j) else { continue };
            let t = l.trim_start();
            if t.starts_with(".method ") {
                header_row = Some(j);
                break;
            }
            if t.starts_with(".end method") {
                // Past the previous method's tail — not in a body.
                return false;
            }
        }
        let Some(header_row) = header_row else { return false };
        if row_index == header_row {
            return false;
        }
        // Don't open an editor on `.end method` — that line is
        // structural; the user can use the method popover or the
        // external editor for big changes.
        if line_text.trim_start().starts_with(".end method") {
            return false;
        }
        let line_offset_within_method = row_index - header_row;
        // Resolve the artifact + (name, sig) of the method via
        // the row's scope mask. Cheap to recompute here so we
        // don't have to thread the mask through the call.
        let scope = crate::smali_row_scope::compute(lines.as_slice());
        let Some(crate::smali_row_scope::RowScope::Method { name, signature }) =
            scope.get(row_index)
        else {
            return false;
        };
        let method_name = name.clone();
        let method_signature_jni = signature.clone();
        // Recover the artifact id from `smali_classes`.
        let Some(bundle) = self.bundle() else { return false };
        let Some(artifact) = bundle.smali_classes.keys().find_map(|(aid, jni)| {
            if jni == &class_jni { Some(aid.clone()) } else { None }
        }) else {
            return false;
        };
        let initial = line_text.trim_start_matches(['\t', ' ']).to_string();
        self.op_edit = Some(crate::op_editor::OpEditState {
            artifact,
            class_jni,
            method_name,
            method_signature_jni,
            row_index,
            line_offset_within_method,
            is_new_line: false,
            input: crate::text_input::TextInput::from_text(initial),
            error: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
        });
        cx.notify();
        self.refresh_op_edit_suggestions(cx);
        true
    }

    pub(crate) fn cancel_op_edit(&mut self, cx: &mut Context<Self>) {
        if self.op_edit.take().is_some() {
            cx.notify();
        }
    }

    /// Common path for Enter (replace in place) and Cmd-Enter
    /// (insert below). `insert_after` distinguishes the two.
    /// Shift every `OpIndex` annotation on `(artifact, class,
    /// method_decl)` whose `op_index >= shift_from` by `delta`.
    /// Used by the per-op editor when inserting or deleting an
    /// op shifts later ops' indices.
    ///
    /// Persists through `glass_db` and refreshes the in-memory
    /// index for the artifact in one go.
    fn shift_op_index_annotations(
        &mut self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        method_decl: &str,
        shift_from: u32,
        delta: i64,
        _cx: &mut Context<Self>,
    ) {
        let Some(db) = self.db_ref() else { return };
        let Ok(entries) = db.load_annotations(artifact) else { return };
        for (key, ann) in entries {
            if let glass_db::AnnotationKey::OpIndex {
                class_jni: ck,
                method_decl: mk,
                op_index,
            } = &key
            {
                if ck != class_jni || mk != method_decl || *op_index < shift_from {
                    continue;
                }
                let new_idx_i64 = *op_index as i64 + delta;
                if new_idx_i64 < 0 {
                    // Deletion swallowed the slot — drop the
                    // annotation entirely.
                    db.clear_annotation(artifact.clone(), key);
                    continue;
                }
                let new_idx = new_idx_i64 as u32;
                let new_key = glass_db::AnnotationKey::OpIndex {
                    class_jni: ck.clone(),
                    method_decl: mk.clone(),
                    op_index: new_idx,
                };
                // Order matters: clear old before set new, in
                // case shift collapses two distinct keys into the
                // same one (shouldn't happen with a single
                // insertion, but cheap to be safe).
                db.clear_annotation(artifact.clone(), key);
                db.set_annotation(artifact.clone(), new_key, ann);
            }
        }
        let _ = db.flush();
        // Rebuild this artifact's in-memory index from the
        // freshly-updated DB so the smali tab re-renders with
        // the right dots / colours.
        let _ = self.refresh_artifact_annotations(artifact);
    }

    fn finish_op_edit(&mut self, insert_after: bool, cx: &mut Context<Self>) {
        let Some(state) = self.op_edit.as_ref() else { return };
        let artifact = state.artifact.clone();
        let class_jni = state.class_jni.clone();
        let method_name = state.method_name.clone();
        let method_signature_jni = state.method_signature_jni.clone();
        let line_offset = state.line_offset_within_method;
        let row_index = state.row_index;
        let new_line = state.input.text().to_string();
        // Locate the original method on the staged-or-original
        // class, splice the user's line in, round-trip via a
        // synthetic class, then write the new ops back onto the
        // real class.
        let mut staged = match self.staged_or_original_class(&artifact, &class_jni) {
            Some(c) => c,
            None => return,
        };
        let method_idx = match staged.methods.iter().position(|m| {
            m.name == method_name
                && m.signature.to_jni() == method_signature_jni
        }) {
            Some(i) => i,
            None => return,
        };
        let method_text = staged.methods[method_idx].to_string();
        let new_body = crate::op_editor::splice_method_body(
            &method_text,
            line_offset,
            &new_line,
            insert_after,
        );
        let wrapper = crate::op_editor::wrap_in_synthetic_class(&new_body, &class_jni);
        let parsed = match glass_api::parse_smali_class(&wrapper) {
            Ok(c) => c,
            Err(e) => {
                self.set_op_edit_error(format!("{e:#}"), cx);
                return;
            }
        };
        // The synthetic class contains exactly one method.
        let Some(new_method) = parsed.methods.into_iter().next() else {
            self.set_op_edit_error(
                "parsed body had no methods (smali parser quirk?)".to_string(),
                cx,
            );
            return;
        };
        // Preserve the original method's identifying metadata so
        // subsequent lookups by (name, signature) still resolve.
        let original = staged.methods[method_idx].clone();
        let old_op_count = original.ops.len();
        let new_op_count = new_method.ops.len();
        let method_decl = format!("{}{}", original.name, original.signature.to_jni());
        // Locate the op the edit landed on *before* we move
        // `original` into the assignment below — we need its
        // unchanged shape to map the user's line offset back
        // to an op index.
        let edited_op_index = crate::annotations::line_offset_to_op_index(
            &original,
            line_offset as u32,
        )
        .unwrap_or(0);
        staged.methods[method_idx] = smali::types::SmaliMethod {
            name: original.name,
            modifiers: original.modifiers,
            constructor: original.constructor,
            signature: original.signature,
            locals: new_method.locals,
            registers: new_method.registers.or(original.registers),
            params: original.params,
            annotations: original.annotations,
            ops: new_method.ops,
        };
        // Re-key OpIndex annotations whose indices shifted. For
        // a pure replace, `delta == 0` and there's nothing to
        // do. Insert-after raises the count by 1; a future
        // delete path would lower it.
        let delta: i64 = new_op_count as i64 - old_op_count as i64;
        if delta != 0 {
            // Insert-after pushes everything from edited_op + 1
            // onwards by `delta`. Delete would remove the slot
            // at edited_op itself; same shift formula.
            let shift_from = if delta > 0 {
                edited_op_index.saturating_add(1)
            } else {
                edited_op_index
            };
            self.shift_op_index_annotations(
                &artifact,
                &class_jni,
                &method_decl,
                shift_from,
                delta,
                cx,
            );
        }
        self.stage_smali_class_edit(artifact, class_jni, staged, cx);
        // Drop the editor state — the row underneath has just
        // been re-rendered by stage_smali_class_edit invalidating
        // tab.lines, so any inline TextInput would be paired
        // against stale row indices.
        if !insert_after {
            self.op_edit = None;
            cx.notify();
            return;
        }
        // For Cmd-Enter, re-open the editor on the new (blank)
        // line one row below the one we just edited. Lines are
        // re-rendered, so the row index advances by exactly one.
        let new_row = row_index + 1;
        if let Some(state) = self.op_edit.as_mut() {
            state.row_index = new_row;
            state.line_offset_within_method = line_offset + 1;
            state.is_new_line = true;
            state.input = crate::text_input::TextInput::new();
            state.error = None;
            state.suggestions.clear();
            state.suggestion_selected = 0;
        }
        cx.notify();
        self.refresh_op_edit_suggestions(cx);
    }

    pub(crate) fn commit_op_edit(&mut self, cx: &mut Context<Self>) {
        self.finish_op_edit(false, cx);
    }

    pub(crate) fn commit_op_edit_and_insert_below(&mut self, cx: &mut Context<Self>) {
        self.finish_op_edit(true, cx);
    }

    /// Recompute the autocomplete suggestion list for the
    /// editor's current cursor. Called after every keystroke and
    /// cursor move. Cheap enough to do synchronously — the
    /// largest source (per-bundle class list) is filtered by
    /// prefix and capped at 12 entries.
    pub(crate) fn refresh_op_edit_suggestions(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.op_edit.as_ref() else { return };
        let ctx = crate::op_editor::classify_cursor(
            state.input.text(),
            state.input.cursor(),
        );
        let suggestions = self.build_op_suggestions(ctx, &state.class_jni);
        if let Some(state) = self.op_edit.as_mut() {
            state.suggestions = suggestions;
            if state.suggestion_selected >= state.suggestions.len() {
                state.suggestion_selected = 0;
            }
        }
        cx.notify();
    }

    /// Build a suggestion list for `ctx`. Pure: doesn't touch
    /// `self.op_edit`. Bundle-aware sources walk the loaded
    /// classes / methods / fields; static sources (opcodes,
    /// registers) are hard-coded.
    fn build_op_suggestions(
        &self,
        ctx: crate::op_editor::OpCursorContext,
        active_class_jni: &str,
    ) -> Vec<crate::op_editor::OpSuggestion> {
        use crate::op_editor::{OpCursorContext, OpSuggestion, OpSuggestionKind};
        const MAX: usize = 50;
        match ctx {
            OpCursorContext::None => Vec::new(),
            OpCursorContext::Opcode { partial, replace_range } => {
                crate::op_editor::OPCODE_LIST
                    .iter()
                    .filter(|m| m.starts_with(&partial))
                    .take(MAX)
                    .map(|m| OpSuggestion {
                        label: SharedString::from(m.to_string()),
                        detail: SharedString::from("opcode"),
                        commit_text: m.to_string(),
                        replace_range,
                        kind: OpSuggestionKind::Opcode,
                    })
                    .collect()
            }
            OpCursorContext::Register { partial, replace_range } => {
                let mut out = Vec::new();
                for i in 0..=15u8 {
                    let name = format!("v{i}");
                    if name.starts_with(&partial) {
                        out.push(OpSuggestion {
                            label: SharedString::from(name.clone()),
                            detail: SharedString::from("local"),
                            commit_text: name,
                            replace_range,
                            kind: OpSuggestionKind::Register,
                        });
                    }
                }
                for i in 0..=7u8 {
                    let name = format!("p{i}");
                    if name.starts_with(&partial) {
                        out.push(OpSuggestion {
                            label: SharedString::from(name.clone()),
                            detail: SharedString::from("param"),
                            commit_text: name,
                            replace_range,
                            kind: OpSuggestionKind::Register,
                        });
                    }
                }
                out
            }
            OpCursorContext::Type { partial, replace_range } => {
                let Some(bundle) = self.bundle() else { return Vec::new() };
                let mut out: Vec<OpSuggestion> = bundle
                    .smali_classes
                    .keys()
                    .filter_map(|(_aid, jni)| {
                        if jni.starts_with(&partial) {
                            Some(OpSuggestion {
                                label: SharedString::from(jni.clone()),
                                detail: SharedString::from("internal class"),
                                commit_text: jni.clone(),
                                replace_range,
                                kind: OpSuggestionKind::Type,
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                // Common Java/Android types as a fallback — these
                // aren't in the loaded DEX but typed-by-hand
                // refs to them are very common.
                for stock in COMMON_EXTERNAL_TYPES {
                    if stock.starts_with(&partial)
                        && !out.iter().any(|s| s.commit_text == *stock)
                    {
                        out.push(OpSuggestion {
                            label: SharedString::from(*stock),
                            detail: SharedString::from("stdlib"),
                            commit_text: stock.to_string(),
                            replace_range,
                            kind: OpSuggestionKind::Type,
                        });
                    }
                }
                out.sort_by(|a, b| a.label.cmp(&b.label));
                out.truncate(MAX);
                out
            }
            OpCursorContext::MethodRef {
                class_jni,
                partial,
                replace_range,
            } => {
                let class = class_jni
                    .as_deref()
                    .unwrap_or(active_class_jni);
                self.suggestions_for_method_ref(class, &partial, replace_range, MAX)
            }
            OpCursorContext::FieldRef {
                class_jni,
                partial,
                replace_range,
            } => {
                let class = class_jni
                    .as_deref()
                    .unwrap_or(active_class_jni);
                self.suggestions_for_field_ref(class, &partial, replace_range, MAX)
            }
        }
    }

    fn suggestions_for_method_ref(
        &self,
        class_jni: &str,
        partial: &str,
        replace_range: (usize, usize),
        max: usize,
    ) -> Vec<crate::op_editor::OpSuggestion> {
        use crate::op_editor::{OpSuggestion, OpSuggestionKind};
        let Some(bundle) = self.bundle() else { return Vec::new() };
        // Find the class — prefer the staged version so newly
        // added methods show up immediately.
        let class = bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
            if jni == class_jni {
                let staged = bundle.smali_edits.get(aid, jni).map(|e| e.modified.clone());
                Some(staged.unwrap_or_else(|| c.clone()))
            } else {
                None
            }
        });
        let Some(class) = class else { return Vec::new() };
        class
            .methods
            .iter()
            .filter_map(|m| {
                let display = format!("{}{}", m.name, m.signature.to_jni());
                if display.starts_with(partial) {
                    Some(OpSuggestion {
                        label: SharedString::from(display.clone()),
                        detail: SharedString::from("method"),
                        commit_text: display,
                        replace_range,
                        kind: OpSuggestionKind::MethodRef,
                    })
                } else {
                    None
                }
            })
            .take(max)
            .collect()
    }

    fn suggestions_for_field_ref(
        &self,
        class_jni: &str,
        partial: &str,
        replace_range: (usize, usize),
        max: usize,
    ) -> Vec<crate::op_editor::OpSuggestion> {
        use crate::op_editor::{OpSuggestion, OpSuggestionKind};
        let Some(bundle) = self.bundle() else { return Vec::new() };
        let class = bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
            if jni == class_jni {
                let staged = bundle.smali_edits.get(aid, jni).map(|e| e.modified.clone());
                Some(staged.unwrap_or_else(|| c.clone()))
            } else {
                None
            }
        });
        let Some(class) = class else { return Vec::new() };
        class
            .fields
            .iter()
            .filter_map(|f| {
                let display = format!("{}:{}", f.name, f.signature.to_jni());
                if display.starts_with(partial) {
                    Some(OpSuggestion {
                        label: SharedString::from(display.clone()),
                        detail: SharedString::from("field"),
                        commit_text: display,
                        replace_range,
                        kind: OpSuggestionKind::FieldRef,
                    })
                } else {
                    None
                }
            })
            .take(max)
            .collect()
    }

    /// Accept the currently-highlighted suggestion. Splices its
    /// `commit_text` into the input over the suggestion's
    /// `replace_range`. No-op if there's no suggestion list.
    pub(crate) fn accept_op_edit_suggestion(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.op_edit.as_mut() else { return };
        let Some(sugg) = state.suggestions.get(state.suggestion_selected).cloned()
        else {
            return;
        };
        let text = state.input.text().to_string();
        let (start, end) = sugg.replace_range;
        let start = start.min(text.len());
        let end = end.min(text.len()).max(start);
        let mut new_text = String::with_capacity(
            text.len() - (end - start) + sugg.commit_text.len(),
        );
        new_text.push_str(&text[..start]);
        new_text.push_str(&sugg.commit_text);
        new_text.push_str(&text[end..]);
        let new_cursor = start + sugg.commit_text.len();
        state.input.set_text(new_text);
        state.input.set_cursor_pos(new_cursor, false);
        state.error = None;
        cx.notify();
        // Refresh — the new cursor may have entered a different
        // context (e.g. after picking an opcode the next slot is
        // a register).
        self.refresh_op_edit_suggestions(cx);
    }

    /// Click handler for the dropdown rows — selects the row
    /// and accepts it in one shot.
    pub(crate) fn click_op_edit_suggestion(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.op_edit.as_mut() {
            if index < state.suggestions.len() {
                state.suggestion_selected = index;
            }
        }
        self.accept_op_edit_suggestion(cx);
    }

    fn set_op_edit_error(&mut self, msg: String, cx: &mut Context<Self>) {
        if let Some(state) = self.op_edit.as_mut() {
            state.error = Some(msg);
        }
        cx.notify();
    }

    /// Helper: return a clone of the staged class for `(artifact,
    /// class_jni)` if any, else the original lifted class.
    /// Returns `None` if neither is loaded.
    fn staged_or_original_class(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
    ) -> Option<smali::types::SmaliClass> {
        let bundle = self.bundle()?;
        bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            })
    }
}
