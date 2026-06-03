//! Hook lifecycle + trace lifecycle, plus the patched-bundle
//! export pipeline they share with the Frida inject path.
//!
//! Lifted out of `shell_actions.rs` so the file stays under the
//! module-size discipline. The methods are still defined on
//! `Shell` via a sibling `impl Shell` block.

use gpui::Context;

use crate::Shell;

impl Shell {
    // ---- Hook lifecycle ------------------------------------------------

    pub(crate) fn toggle_hooks_dialog(&mut self, cx: &mut Context<Self>) {
        self.hooks_dialog_open = !self.hooks_dialog_open;
        // Always start in list mode — close any editor if it
        // was left open from a previous session.
        self.hook_editor_target = None;
        self.hook_editor_buffer.clear();
        cx.notify();
    }

    pub(crate) fn close_hooks_dialog(&mut self, cx: &mut Context<Self>) {
        if self.hooks_dialog_open {
            self.hooks_dialog_open = false;
            self.hook_editor_target = None;
            self.hook_editor_buffer.clear();
            cx.notify();
        }
    }

    // The four hook-editor methods below wire a future
    // multi-line JS editor pane into the Hooks dialog. The
    // text-input widget is single-line today; once we grow a
    // multi-line variant the dialog's "Edit" button will
    // call open_hook_editor and the editor's commit handler
    // will call save_hook_editor. Leaving the plumbing in
    // place — it's the right shape — but suppressing the
    // dead-code warning until the UI surface exists.
    #[allow(dead_code)]
    /// Switch the hooks dialog into editor mode for one key.
    /// Pre-fills the buffer with the entry's existing JS body
    /// (or a sensible default) so the user can iterate.
    pub(crate) fn open_hook_editor(
        &mut self,
        key: crate::hooks::HookKey,
        cx: &mut Context<Self>,
    ) {
        let initial = self
            .bundle()
            .and_then(|b| b.hooks.get(&key))
            .map(|e| match &e.action {
                crate::hooks::HookAction::CustomJs(body) => body.clone(),
                crate::hooks::HookAction::ReturnLiteral(lit) => {
                    format!("return {lit};")
                }
                crate::hooks::HookAction::LogOnly => {
                    "// runs after the original — return its value\n\
                     return originalImpl.apply(this, args);"
                        .to_string()
                }
            })
            .unwrap_or_else(|| {
                "// args[] are the call's parameters\n\
                 // call originalImpl.apply(this, args) to invoke the\n\
                 // real method, or return a value to override.\n\
                 return originalImpl.apply(this, args);"
                    .to_string()
            });
        self.hook_editor_target = Some(key);
        self.hook_editor_buffer = initial;
        cx.notify();
    }

    #[allow(dead_code)]
    pub(crate) fn close_hook_editor(&mut self, cx: &mut Context<Self>) {
        self.hook_editor_target = None;
        self.hook_editor_buffer.clear();
        cx.notify();
    }

    #[allow(dead_code)]
    /// Persist the editor buffer as a CustomJs hook on the
    /// editor's target key. If the hook doesn't exist yet
    /// (user is creating it fresh), it's started; otherwise
    /// the existing script is unloaded and a new one created
    /// with the new body.
    pub(crate) fn save_hook_editor(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.hook_editor_target.clone() else { return };
        let body = self.hook_editor_buffer.clone();
        // Stop the running hook (if any), then start a fresh
        // one with the new body. This is the simplest "edit"
        // path — Frida sessions don't support live script
        // mutation, so create-replace-on-edit is the model.
        let exists = self
            .bundle()
            .map(|b| b.hooks.get(&key).is_some())
            .unwrap_or(false);
        if exists {
            self.stop_hook(
                key.artifact.clone(),
                key.class_jni.clone(),
                key.method_name.clone(),
                key.method_signature.clone(),
                cx,
            );
        }
        self.start_hook(
            key.artifact.clone(),
            key.class_jni.clone(),
            key.method_name.clone(),
            key.method_signature.clone(),
            crate::hooks::HookAction::CustomJs(body),
            cx,
        );
        self.close_hook_editor(cx);
    }

    #[allow(dead_code)]
    /// Track the editor's buffer. Called by the multi-line
    /// text input on every keystroke.
    pub(crate) fn set_hook_editor_buffer(
        &mut self,
        text: String,
        cx: &mut Context<Self>,
    ) {
        self.hook_editor_buffer = text;
        cx.notify();
    }

    pub(crate) fn start_hook(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature: String,
        action: crate::hooks::HookAction,
        cx: &mut Context<Self>,
    ) {
        let Some(dock) = self.debug_dock.as_ref() else {
            return;
        };
        let Some(session) = dock.session.clone() else {
            self.push_dock_log("• not attached — connect first", cx);
            return;
        };
        let key = crate::hooks::HookKey {
            artifact: artifact.clone(),
            class_jni: class_jni.clone(),
            method_name: method_name.clone(),
            method_signature: method_signature.clone(),
        };
        if let Some(bundle) = self.bundle() {
            if let Some(existing) = bundle.hooks.get(&key) {
                if matches!(
                    existing.status,
                    crate::hooks::HookStatus::Pending
                        | crate::hooks::HookStatus::Active
                ) {
                    self.push_dock_log(
                        format!("• already hooking {class_jni}.{method_name}"),
                        cx,
                    );
                    return;
                }
            }
        }
        if let Some(bundle) = self.bundle_mut() {
            bundle.hooks.remove(&key);
        }
        let body = match &action {
            crate::hooks::HookAction::LogOnly => glass_frida::HookBody::LogOnly,
            crate::hooks::HookAction::ReturnLiteral(lit) => {
                glass_frida::HookBody::ReturnLiteral(lit.clone())
            }
            crate::hooks::HookAction::CustomJs(body) => {
                glass_frida::HookBody::Custom(body.clone())
            }
        };
        let js = match glass_frida::render_hook_script(
            &class_jni,
            &method_name,
            &method_signature,
            &body,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.push_dock_log(format!("• render failed: {e}"), cx);
                return;
            }
        };
        let script_id = session.alloc_script_id();
        if let Some(bundle) = self.bundle_mut() {
            bundle.hooks.insert(crate::hooks::HookEntry {
                key: key.clone(),
                script_id: Some(script_id),
                status: crate::hooks::HookStatus::Pending,
                action,
                created_at: std::time::Instant::now(),
                invocations: Vec::new(),
            });
        }
        self.push_dock_log(
            format!("⚙ hooking {class_jni}.{method_name}{method_signature}"),
            cx,
        );
        cx.notify();
        let name = format!(
            "hook-{}-{}",
            class_jni.replace('/', "."),
            method_name
        );
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    session.create_script(script_id, name, js)
                })
                .await;
            let _ = this.update(cx, |shell, cx| match result {
                Ok(()) => {
                    if let Some(bundle) = shell.bundle_mut() {
                        bundle.hooks.mark_active(&key, script_id);
                    }
                    shell.push_dock_log(
                        format!("• hook {script_id} active"),
                        cx,
                    );
                }
                Err(e) => {
                    if let Some(bundle) = shell.bundle_mut() {
                        bundle.hooks.mark_failed(&key, e.clone());
                    }
                    shell.push_dock_log(
                        format!("• hook {script_id} failed: {e}"),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    pub(crate) fn stop_hook(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature: String,
        cx: &mut Context<Self>,
    ) {
        let key = crate::hooks::HookKey {
            artifact,
            class_jni: class_jni.clone(),
            method_name: method_name.clone(),
            method_signature,
        };
        let script_id = self
            .bundle()
            .and_then(|b| b.hooks.get(&key))
            .and_then(|e| e.script_id);
        if let Some(bundle) = self.bundle_mut() {
            bundle.hooks.remove(&key);
        }
        self.push_dock_log(
            format!("◼ stop hook {class_jni}.{method_name}"),
            cx,
        );
        let Some(session) = self
            .debug_dock
            .as_ref()
            .and_then(|d| d.session.clone())
        else {
            return;
        };
        let Some(id) = script_id else { return };
        cx.spawn(async move |_this, cx| {
            let _ = cx
                .background_executor()
                .spawn(async move {
                    let _ = session.unload_script(id);
                })
                .await;
        })
        .detach();
    }

    pub(crate) fn stop_all_hooks(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<crate::hooks::HookKey> = self
            .bundle()
            .map(|b| b.hooks.entries().iter().map(|e| e.key.clone()).collect())
            .unwrap_or_default();
        for k in keys {
            self.stop_hook(
                k.artifact,
                k.class_jni,
                k.method_name,
                k.method_signature,
                cx,
            );
        }
    }

    /// Smoke test: load a tiny script that calls `send(1+1)`
    /// in the gadget. If the wiring works, the dock's event
    /// pump turns this into a log line like
    /// `[script <id>] 2` within a tick or two. Used to verify
    /// the M3.4 plumbing without any feature code on top.
    pub(crate) fn debug_smoke_test(&mut self, cx: &mut Context<Self>) {
        let Some(dock) = self.debug_dock.as_ref() else { return };
        let Some(session) = dock.session.clone() else {
            self.push_dock_log("not connected — try Connect first", cx);
            return;
        };
        let id = session.alloc_script_id();
        self.push_dock_log(format!("• loading smoke-test script {id}"), cx);
        // Background the create-script call since it blocks
        // on the actor thread; keeps the UI responsive.
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // Diagnostic probe: enumerate the gadget's
                    // global scope so we can see what bridges
                    // are actually present. Sends three lines:
                    //   * runtime + frida.version
                    //   * the global keys (Module, Process,
                    //     Java, ObjC, …)
                    //   * any Java-like candidates we spotted
                    // Smoke test: splice a tiny diagnostic
                    // into the bridge bundle. The same code
                    // path trace/hook scripts use. If this
                    // works, traces will work.
                    let user = r#"
                        send({
                          kind: 'info',
                          stage: 'smoke-after-bridge',
                          typeofJava: typeof Java,
                          javaAvailable: typeof Java !== 'undefined' && Java.available,
                        });
                    "#;
                    let src = glass_frida::build_bridged_script(user);
                    session.create_script(id, "glass-smoke-bridged", src)
                })
                .await;
            let line = match result {
                Ok(()) => format!("smoke script {id} loaded"),
                Err(e) => format!("smoke script {id} failed: {e}"),
            };
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log(line, cx);
            });
        })
        .detach();
    }

    /// Start tracing a Java method on the connected gadget.
    /// Inserts a `Pending` entry into the bundle's trace
    /// registry, renders the Frida JS, allocates a script id,
    /// and asks the session actor to load it. On load success
    /// the entry flips to `Active`; on failure to `Failed`.
    ///
    /// No-op (with a log line) when:
    ///   * No bundle is loaded.
    ///   * The dock isn't open / not attached.
    ///   * The method is already being traced.
    pub(crate) fn start_trace(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature: String,
        cx: &mut Context<Self>,
    ) {
        let Some(dock) = self.debug_dock.as_ref() else {
            tracing::info!("start_trace: dock not open — connect first");
            return;
        };
        let Some(session) = dock.session.clone() else {
            self.push_dock_log("• not attached — connect first", cx);
            return;
        };
        let key = crate::traces::TraceKey {
            artifact: artifact.clone(),
            class_jni: class_jni.clone(),
            method_name: method_name.clone(),
            method_signature: method_signature.clone(),
        };
        // Refuse to double-trace ONLY when an active or
        // pending trace is live. Failed / Stopped entries
        // are eligible for retry — the user already saw
        // the failure and clicked Trace again to retry,
        // so let them.
        if let Some(bundle) = self.bundle() {
            if let Some(existing) = bundle.traces.get(&key) {
                if matches!(
                    existing.status,
                    crate::traces::TraceStatus::Pending
                        | crate::traces::TraceStatus::Active
                ) {
                    self.push_dock_log(
                        format!("• already tracing {class_jni}.{method_name}"),
                        cx,
                    );
                    return;
                }
            }
        }
        // Drop any prior Failed/Stopped entry so the
        // insert below replaces it cleanly. mark_failed
        // doesn't update by_script, but remove is the
        // canonical way to clear both indices.
        if let Some(bundle) = self.bundle_mut() {
            bundle.traces.remove(&key);
        }
        // Render JS up front so we fail fast on a bad signature.
        let js = match glass_frida::render_trace_script(
            &class_jni,
            &method_name,
            &method_signature,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.push_dock_log(
                    format!("• render failed: {e}"),
                    cx,
                );
                return;
            }
        };
        let script_id = session.alloc_script_id();
        // Stage Pending → caller's pane can show a "loading"
        // indicator until the actor confirms.
        if let Some(bundle) = self.bundle_mut() {
            bundle.traces.insert(crate::traces::TraceEntry {
                key: key.clone(),
                script_id: Some(script_id),
                status: crate::traces::TraceStatus::Pending,
                created_at: std::time::Instant::now(),
                invocations: Vec::new(),
            });
        }
        self.push_dock_log(
            format!("▶ tracing {class_jni}.{method_name}{method_signature}"),
            cx,
        );
        cx.notify();
        // Load the script off the foreground.
        let name = format!(
            "trace-{}-{}",
            class_jni.replace('/', "."),
            method_name
        );
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    session.create_script(script_id, name, js)
                })
                .await;
            let _ = this.update(cx, |shell, cx| match result {
                Ok(()) => {
                    if let Some(bundle) = shell.bundle_mut() {
                        bundle.traces.mark_active(&key, script_id);
                    }
                    shell.push_dock_log(
                        format!("• trace {script_id} active"),
                        cx,
                    );
                }
                Err(e) => {
                    if let Some(bundle) = shell.bundle_mut() {
                        bundle.traces.mark_failed(&key, e.clone());
                    }
                    shell.push_dock_log(
                        format!("• trace {script_id} failed: {e}"),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Stop and unregister a trace. Removes the entry from
    /// the registry and asks the actor to unload the script.
    /// Cheap when the trace is already gone.
    pub(crate) fn stop_trace(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature: String,
        cx: &mut Context<Self>,
    ) {
        let key = crate::traces::TraceKey {
            artifact,
            class_jni: class_jni.clone(),
            method_name: method_name.clone(),
            method_signature,
        };
        // Pull the script id out before we drop the entry.
        let script_id = self
            .bundle()
            .and_then(|b| b.traces.get(&key))
            .and_then(|e| e.script_id);
        if let Some(bundle) = self.bundle_mut() {
            bundle.traces.remove(&key);
        }
        self.push_dock_log(
            format!("◼ stop trace {class_jni}.{method_name}"),
            cx,
        );
        let Some(session) = self
            .debug_dock
            .as_ref()
            .and_then(|d| d.session.clone())
        else {
            return;
        };
        let Some(id) = script_id else { return };
        cx.spawn(async move |_this, cx| {
            let _ = cx
                .background_executor()
                .spawn(async move {
                    let _ = session.unload_script(id);
                })
                .await;
        })
        .detach();
    }

    pub(crate) fn export_patched_bundle(&mut self, cx: &mut Context<Self>) {
        use std::collections::HashMap;
        let Some(bundle) = self.bundle() else { return };
        if bundle.edits.is_empty()
            && bundle.smali_edits.is_empty()
            && bundle.pending_additions.is_empty()
            && bundle.plist_edits.is_empty()
            && bundle.manifest_edits.is_empty()
        {
            return;
        }
        // Build the EditMap up-front (cheap clone of edit
        // metadata) so the post-dialog continuation doesn't need
        // to reach back into the bundle.
        let mut edit_map: HashMap<glass_db::ArtifactId, Vec<glass_api::EditPatch>> =
            HashMap::new();
        for e in bundle.edits.entries() {
            edit_map.entry(e.artifact.clone()).or_default().push(
                glass_api::EditPatch {
                    vaddr: e.vaddr,
                    new_bytes: e.new_bytes.clone(),
                },
            );
        }
        // Parallel map for typed smali class edits, keyed by DEX
        // artifact id (matches the loader's hashing of raw DEX
        // bytes).
        let mut smali_edit_map: glass_api::SmaliEditMap = HashMap::new();
        for e in bundle.smali_edits.entries() {
            smali_edit_map
                .entry(e.key.artifact.clone())
                .or_default()
                .insert(e.key.class_jni.clone(), e.modified.clone());
        }
        // Pending APK additions (new zip entries): clone the
        // bundle's map up front so the post-prompt continuation
        // doesn't need a borrow on Shell.
        let additions: glass_api::ApkAdditions = bundle.pending_additions.clone();
        // Plist edits: archive_path → serialised bytes
        // (already in original on-disk format). Resolved via
        // `plist_sources` so we know where to splice.
        let mut plist_edit_map: glass_api::PlistEditMap =
            std::collections::BTreeMap::new();
        for e in bundle.plist_edits.entries() {
            if let Some((archive_path, _orig)) =
                bundle.plist_sources.get(&e.artifact)
            {
                plist_edit_map.insert(archive_path.clone(), e.bytes.clone());
            }
        }
        // Manifest edits: same shape as plist edits — archive_path
        // → serialised binary AXML bytes — resolved through
        // `manifest_sources`.
        let mut manifest_edit_map: glass_api::ManifestEditMap =
            std::collections::BTreeMap::new();
        for e in bundle.manifest_edits.entries() {
            if let Some((archive_path, _orig)) =
                bundle.manifest_sources.get(&e.artifact)
            {
                manifest_edit_map.insert(archive_path.clone(), e.bytes.clone());
            }
        }
        // Re-load the source bundle from disk so the exporter
        // sees fresh bytes (the in-memory ParsedArtifact is the
        // source of truth for which file to patch, but the
        // exporter wants a Bundle handle anyway for the path).
        let source_path = self
            .source_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let suggested = patched_filename(&source_path);
        let dir = source_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let rx = {
            let app: &mut gpui::App = &mut *cx;
            app.prompt_for_new_path(&dir, Some(&suggested))
        };
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(out_path))) = rx.await else { return };
            // Flip the progress flag + close the dialog so the
            // overlay takes over.
            let _ = this.update(cx, |shell, cx| {
                shell.export_in_progress = true;
                shell.changes_dialog_open = false;
                shell.export_status = None;
                cx.notify();
            });
            // Animation pump: tick at ~30fps so the indeterminate
            // bar slides while the heavy work runs. Stops on its
            // own when `export_in_progress` flips false.
            {
                let this_pump = this.clone();
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(33))
                            .await;
                        let still_running = this_pump
                            .update(cx, |shell, cx| {
                                cx.notify();
                                shell.export_in_progress
                            })
                            .unwrap_or(false);
                        if !still_running {
                            break;
                        }
                    }
                })
                .detach();
            }
            // Re-open + export off the foreground thread. The
            // background_executor pool is the right home — gpui's
            // main runloop stays responsive while we splice the
            // archive.
            let edit_map_for_task = edit_map.clone();
            let smali_map_for_task = smali_edit_map.clone();
            let additions_for_task = additions.clone();
            let plist_map_for_task = plist_edit_map.clone();
            let manifest_map_for_task = manifest_edit_map.clone();
            let source_path_for_task = source_path.clone();
            let out_path_for_task = out_path.clone();
            let summary = cx
                .background_executor()
                .spawn(async move {
                    match glass_api::open(&source_path_for_task) {
                        Ok(bundle) => match glass_api::export_to_path_full(
                            &bundle,
                            &edit_map_for_task,
                            &smali_map_for_task,
                            &additions_for_task,
                            &plist_map_for_task,
                            &manifest_map_for_task,
                            &out_path_for_task,
                        ) {
                            Ok(()) => Ok(out_path_for_task),
                            Err(e) => Err(format!("{e:#}")),
                        },
                        Err(e) => Err(format!("re-open failed: {e:#}")),
                    }
                })
                .await;
            match &summary {
                Ok(p) => tracing::info!("exported patched bundle to {}", p.display()),
                Err(e) => tracing::warn!("export failed: {e}"),
            }
            let _ = this.update(cx, |shell, cx| {
                shell.export_in_progress = false;
                shell.export_status = Some(summary);
                cx.notify();
            });
        })
        .detach();
    }

    /// Close the dialog and jump to the edit's address. Picks
    /// the listing view for text-section addresses, the hex
    /// view for data-section addresses (matching where each
    /// kind of edit was originally staged).
    pub(crate) fn navigate_to_disasm_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        vaddr: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle() else { return };
        let target = bundle
            .text_section_for_addr(&artifact, vaddr)
            .map(|s| (s.to_string(), true))
            .or_else(|| {
                bundle
                    .data_section_for_addr(&artifact, vaddr)
                    .map(|s| (s.to_string(), false))
            });
        let Some((section, is_text)) = target else { return };
        self.changes_dialog_open = false;
        self.changes_dialog_confirm_abandon = false;
        if is_text {
            self.open_listing_in_new_tab(artifact, section, vaddr, cx);
        } else {
            self.open_hex_in_new_tab(artifact, section, vaddr, cx);
        }
    }

    pub(crate) fn bundle_mut(&mut self) -> Option<&mut crate::LoadedBundle> {
        if let crate::ShellState::Ready(b) = &mut self.state {
            Some(b)
        } else {
            None
        }
    }
}

/// APK / IPA / `.so` to downstream tools) and insert
/// `-patched` before it.
fn patched_filename(source: &std::path::Path) -> String {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("patched");
    let ext = source.extension().and_then(|s| s.to_str()).unwrap_or("");
    if ext.is_empty() {
        format!("{stem}-patched")
    } else {
        format!("{stem}-patched.{ext}")
    }
}
