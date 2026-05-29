//! Debug dock — the bottom panel that owns the live Frida
//! `Session` for the connected device + app.
//!
//! Lifted out of `shell_actions.rs` so the file stays under the
//! module-size discipline. The methods are still defined on
//! `Shell` via a sibling `impl Shell` block.

use gpui::Context;

use crate::Shell;

impl Shell {
    // ---- Debug dock ----------------------------------------------------

    /// Open the bottom debug dock against the currently-selected
    /// device + loaded APK. Captures the device snapshot + the
    /// bundle's package name + the latest probe's agent version
    /// at connect time so the dock stays anchored even if the
    /// chip selection changes underneath.
    pub(crate) fn open_debug_dock(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.selected_device.clone() else { return false };
        let Some(device) = self
            .device_snapshot
            .iter()
            .find(|d| d.id == id)
            .cloned()
        else {
            return false;
        };
        let Some(bundle) = self.bundle() else { return false };
        let Some(manifest) = bundle.android_manifest.as_ref() else {
            return false;
        };
        let Some(package) = manifest.package_name().map(|s| s.to_string())
        else {
            return false;
        };
        let agent_version = self
            .frida_probes
            .get(&id)
            .and_then(|c| c.result.as_ref().ok())
            .and_then(|r| r.agent_version.clone());
        // Spawn the Frida session actor up front. We hand it
        // the dock immediately so the UI can render; the
        // actual attach runs on a background task and
        // populates `session` when it completes.
        let session = glass_frida::Session::spawn();
        self.debug_dock = Some(crate::DebugDockState {
            device: device.clone(),
            package: package.clone(),
            agent_version,
            log: vec![format!("connecting to {package}…")],
            height: gpui::px(180.),
            session: Some(session.clone()),
            attaching: true,
        });
        // The dock comes from the picker dropdown — close that
        // so the chip doesn't fight the new dock for attention.
        self.device_picker_open = false;
        cx.notify();
        // Kick off the attach. Two steps off the foreground
        // executor:
        //   1. Resolve the package's PID via `adb shell pidof`.
        //   2. Set up `adb forward tcp:27442 tcp:27042` (the
        //      gadget probe already does this, harmless to
        //      repeat — adb just returns the same port).
        //   3. Ask Frida to add a remote device at the
        //      forwarded address + attach to that PID.
        let device_manager = self.device_manager.clone();
        let serial = device.id.serial.clone();
        let package_for_task = package.clone();
        cx.spawn(async move |this, cx| {
            // Capture the resolved PID alongside the attach
            // outcome so the auto-resume step below can call
            // session.resume(pid).
            let attach_outcome: Result<(glass_frida::AttachReport, u32), String> = cx
                .background_executor()
                .spawn({
                    let session = session.clone();
                    async move {
                        let status = device_manager.backend_status().adb.clone();
                        let Ok(adb_info) = status else {
                            return Err("ADB not available".to_string());
                        };
                        let backend = glass_device::adb::AdbBackend::with_override(
                            Some(adb_info.binary_path.clone()),
                        )
                        .map_err(|e| format!("adb backend: {e}"))?;
                        let pid_out = backend
                            .shell(&serial, &["pidof", &package_for_task])
                            .map_err(|e| format!("pidof: {e}"))?;
                        let pid: u32 = pid_out
                            .split_whitespace()
                            .next()
                            .and_then(|s| s.parse().ok())
                            .ok_or_else(|| {
                                format!("{package_for_task} isn't running on the device — launch it first")
                            })?;
                        let _ = backend.probe_gadget(&serial);
                        let rep = session
                            .attach_remote("127.0.0.1:27442", pid)
                            .map_err(|e| format!("attach {pid}: {e}"))?;
                        Ok((rep, pid))
                    }
                })
                .await;
            // First update: surface the attach result and
            // decide whether we should auto-resume. Empty
            // registries → resume immediately so the user
            // can use the app. Non-empty → leave paused so
            // they can install / verify hooks before letting
            // the app run.
            let (should_resume, pid_to_resume, session_for_resume) =
                this.update(cx, |shell, cx| {
                    // Read registry state up front so we don't
                    // hold a Shell-immutable + dock-mutable
                    // borrow at the same time.
                    let registries_empty = shell
                        .bundle()
                        .map(|b| b.traces.is_empty() && b.hooks.is_empty())
                        .unwrap_or(true);
                    if let Some(dock) = shell.debug_dock.as_mut() {
                        dock.attaching = false;
                        match &attach_outcome {
                            Ok((_, pid)) => {
                                dock.log.push("connected".to_string());
                                let sess = dock.session.clone();
                                cx.notify();
                                (registries_empty, Some(*pid), sess)
                            }
                            Err(e) => {
                                dock.log.push(format!("attach failed: {e}"));
                                if let Some(s) = dock.session.take() {
                                    s.shutdown();
                                }
                                cx.notify();
                                (false, None, None)
                            }
                        }
                    } else {
                        (false, None, None)
                    }
                })
                .unwrap_or((false, None, None));
            if should_resume {
                if let (Some(pid), Some(session)) =
                    (pid_to_resume, session_for_resume)
                {
                    let resume_res = cx
                        .background_executor()
                        .spawn(async move { session.resume(pid) })
                        .await;
                    let _ = this.update(cx, |shell, cx| match resume_res {
                        Ok(()) => {
                            shell.push_dock_log(
                                "▶ auto-resumed (no traces / hooks defined)",
                                cx,
                            );
                        }
                        Err(e) => {
                            shell.push_dock_log(format!("resume failed: {e}"), cx);
                        }
                    });
                }
            } else if pid_to_resume.is_some() {
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(
                        "⏸ paused — click Restart after installing traces / hooks",
                        cx,
                    );
                });
            }
        })
        .detach();
        true
    }

    pub(crate) fn close_debug_dock(&mut self, cx: &mut Context<Self>) {
        if let Some(mut dock) = self.debug_dock.take() {
            if let Some(session) = dock.session.take() {
                // Best-effort detach, then drop everything.
                let _ = session.detach();
                session.shutdown();
            }
            cx.notify();
        }
    }

    /// Stash the current pointer Y and current dock height on
    /// mouse-down so subsequent mouse-moves can resize relative
    /// to a stable anchor instead of accumulating tiny deltas.
    pub(crate) fn start_dock_resize(
        &mut self,
        pointer_y: gpui::Pixels,
        _cx: &mut Context<Self>,
    ) {
        if let Some(dock) = self.debug_dock.as_ref() {
            self.debug_dock_resize_anchor = Some((pointer_y, dock.height));
        }
    }

    /// Apply a drag delta. The pointer moving *up* (smaller Y)
    /// grows the dock; moving down shrinks it.
    pub(crate) fn update_dock_resize(
        &mut self,
        pointer_y: gpui::Pixels,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.debug_dock_resize_anchor
        else {
            return;
        };
        let dy = anchor_y.as_f32() - pointer_y.as_f32();
        let new_h = gpui::px(anchor_h.as_f32() + dy);
        self.set_debug_dock_height(new_h, cx);
    }

    pub(crate) fn finish_dock_resize(&mut self, cx: &mut Context<Self>) {
        if self.debug_dock_resize_anchor.take().is_some() {
            cx.notify();
        }
    }

    /// Set the dock's height. Used by the drag-handle on the
    /// top edge; values are clamped to a sane range.
    pub(crate) fn set_debug_dock_height(
        &mut self,
        h: gpui::Pixels,
        cx: &mut Context<Self>,
    ) {
        if let Some(dock) = self.debug_dock.as_mut() {
            // Lower bound: enough to show the controls + a
            // single log line. Upper bound: half the window
            // (we don't know the window height here, so cap
            // at 800 — windows are typically taller than that
            // and the dock can be re-resized).
            let clamped = h.as_f32().clamp(80.0, 800.0);
            dock.height = gpui::px(clamped);
            cx.notify();
        }
    }

    /// Append a log line to the dock. Trims trailing whitespace
    /// and skips empty lines so the column stays tight.
    pub(crate) fn push_dock_log(&mut self, line: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(dock) = self.debug_dock.as_mut() {
            let line = line.into();
            for s in line.lines() {
                let trimmed = s.trim_end();
                if !trimmed.is_empty() {
                    dock.log.push(trimmed.to_string());
                }
            }
            // Keep the log bounded so a chatty action doesn't
            // OOM the dock. 200 lines = several screens of
            // history, plenty for the play/stop cadence.
            const MAX_LOG: usize = 200;
            if dock.log.len() > MAX_LOG {
                let drop = dock.log.len() - MAX_LOG;
                dock.log.drain(..drop);
            }
            cx.notify();
        }
    }

    /// Launch the dock's package on the dock's device. Runs
    /// `adb shell monkey -p <pkg> -c LAUNCHER 1` off the
    /// foreground; pipes the combined stdout/stderr into the
    /// dock's log column.
    /// Restart-with-hooks orchestrator. One click runs the
    /// whole "give me a fresh app instance with all my
    /// instrumentation in place" workflow:
    ///
    ///   1. `adb shell am force-stop <pkg>`.
    ///   2. `adb shell monkey -p <pkg> -c LAUNCHER 1`.
    ///   3. Poll the gadget port until it answers (gadget
    ///      is in `on_load: wait` so it'll be paused
    ///      inside <clinit>).
    ///   4. Resolve the new PID via `pidof <pkg>`.
    ///   5. Drop the old Frida session and attach to the
    ///      new PID via the same actor.
    ///   6. Re-render every trace + hook script (their
    ///      old script ids point at the dead session, so
    ///      we invalidate them) and load them against the
    ///      paused process.
    ///   7. Call `session.resume(pid)` — gadget unblocks,
    ///      app continues with hooks in place.
    ///
    /// When there are no traces / hooks defined, steps 6
    /// collapses to a no-op so this is also the correct
    /// "just restart the app" button.
    pub(crate) fn debug_restart(&mut self, cx: &mut Context<Self>) {
        let Some(dock) = self.debug_dock.as_ref() else { return };
        let serial = dock.device.id.serial.clone();
        let package = dock.package.clone();
        let device_manager = self.device_manager.clone();
        // Snapshot the trace + hook registries so we can
        // re-install after the new attach. Cloning is cheap
        // (each entry is a few strings + bounded Vec).
        let traces: Vec<crate::traces::TraceEntry> = self
            .bundle()
            .map(|b| b.traces.entries().iter().map(|&e| e.clone()).collect())
            .unwrap_or_default();
        let hooks: Vec<crate::hooks::HookEntry> = self
            .bundle()
            .map(|b| b.hooks.entries().iter().map(|&e| e.clone()).collect())
            .unwrap_or_default();
        // Drop the old session — the process we're about to
        // kill owns it.
        let old_session = self
            .debug_dock
            .as_mut()
            .and_then(|d| d.session.take());
        self.push_dock_log(format!("↻ restarting {package}"), cx);
        cx.spawn(async move |this, cx| {
            // Tear down old session off the main thread.
            if let Some(s) = old_session {
                let _ = cx
                    .background_executor()
                    .spawn(async move {
                        let _ = s.detach();
                        s.shutdown();
                    })
                    .await;
            }
            // ADB backend handle — reused for every step.
            let adb = match cx
                .background_executor()
                .spawn({
                    let dm = device_manager.clone();
                    async move {
                        let status = dm.backend_status().adb.clone()
                            .map_err(|e| format!("adb: {e}"))?;
                        glass_device::adb::AdbBackend::with_override(
                            Some(status.binary_path),
                        )
                        .map_err(|e| format!("adb backend: {e}"))
                    }
                })
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    let _ = this.update(cx, |shell, cx| {
                        shell.push_dock_log(format!("✗ {e}"), cx);
                    });
                    return;
                }
            };
            // 1. force-stop.
            let _ = cx
                .background_executor()
                .spawn({
                    let serial = serial.clone();
                    let package = package.clone();
                    let adb = adb.clone();
                    async move { adb.force_stop(&serial, &package) }
                })
                .await;
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log("• stopped", cx);
            });
            // 2. start.
            let start_res = cx
                .background_executor()
                .spawn({
                    let serial = serial.clone();
                    let package = package.clone();
                    let adb = adb.clone();
                    async move { adb.start_main_activity(&serial, &package) }
                })
                .await;
            if let Err(e) = start_res {
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(format!("✗ start: {e}"), cx);
                });
                return;
            }
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log("• launched", cx);
            });
            // 3. wait for the gadget to come back up. Gadget
            //    is in on_load:wait so it'll bind 27042 inside
            //    <clinit> and block; probe_gadget returns
            //    Ok(true) once that happens. Poll for up to
            //    ~10s; clinit usually fires within 1-2s.
            let gadget_alive = cx
                .background_executor()
                .spawn({
                    let serial = serial.clone();
                    let adb = adb.clone();
                    async move {
                        for _ in 0..50 {
                            if let Ok(true) = adb.probe_gadget(&serial) {
                                return true;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        false
                    }
                })
                .await;
            if !gadget_alive {
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(
                        "✗ gadget never came back up (10s timeout)",
                        cx,
                    );
                });
                return;
            }
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log("• gadget ready", cx);
            });
            // 4. resolve PID.
            let pid_str = cx
                .background_executor()
                .spawn({
                    let serial = serial.clone();
                    let package = package.clone();
                    let adb = adb.clone();
                    async move { adb.shell(&serial, &["pidof", &package]) }
                })
                .await;
            let pid: u32 = match pid_str {
                Ok(s) => match s.split_whitespace().next().and_then(|t| t.parse().ok()) {
                    Some(n) => n,
                    None => {
                        let _ = this.update(cx, |shell, cx| {
                            shell.push_dock_log("✗ couldn't parse pid", cx);
                        });
                        return;
                    }
                },
                Err(e) => {
                    let _ = this.update(cx, |shell, cx| {
                        shell.push_dock_log(format!("✗ pidof: {e}"), cx);
                    });
                    return;
                }
            };
            // 5. fresh actor + attach.
            let session = glass_frida::Session::spawn();
            let attach_res = cx
                .background_executor()
                .spawn({
                    let session = session.clone();
                    async move { session.attach_remote("127.0.0.1:27442", pid) }
                })
                .await;
            if let Err(e) = attach_res {
                session.shutdown();
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(format!("✗ attach pid {pid}: {e}"), cx);
                });
                return;
            }
            let _ = this.update(cx, |shell, cx| {
                if let Some(dock) = shell.debug_dock.as_mut() {
                    dock.session = Some(session.clone());
                    dock.attaching = false;
                }
                shell.push_dock_log(format!("• attached pid {pid}", pid = pid), cx);
                cx.notify();
            });
            // 6. re-install every trace + hook script.
            //    We render fresh JS each time because the
            //    old script ids belong to the dead session.
            //    Errors here don't block resume — we'd
            //    rather get the app running with a partial
            //    set than freeze waiting for one bad trace.
            let mut installed = 0usize;
            let mut failed = 0usize;
            for entry in &traces {
                let new_id = session.alloc_script_id();
                let js = match glass_frida::render_trace_script(
                    &entry.key.class_jni,
                    &entry.key.method_name,
                    &entry.key.method_signature,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        failed += 1;
                        let key = entry.key.clone();
                        let err = format!("{e}");
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.traces.mark_failed(&key, err.clone());
                            }
                            shell.push_dock_log(
                                format!("✗ trace render {}.{}: {err}",
                                    key.class_jni, key.method_name),
                                cx,
                            );
                        });
                        continue;
                    }
                };
                let name = format!(
                    "trace-{}-{}",
                    entry.key.class_jni.replace('/', "."),
                    entry.key.method_name
                );
                let key = entry.key.clone();
                let res = cx
                    .background_executor()
                    .spawn({
                        let session = session.clone();
                        async move { session.create_script(new_id, name, js) }
                    })
                    .await;
                match res {
                    Ok(()) => {
                        installed += 1;
                        let _ = this.update(cx, |shell, _cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                // Clear stale by_script entry
                                // before remapping the key to
                                // the new ScriptId.
                                if let Some(existing) = bundle.traces.get(&key) {
                                    let _ = existing;
                                }
                                // mark_active rewrites by_script
                                // for new_id; we also need to
                                // wipe invocations from the
                                // previous run so the dialog
                                // hit-count resets.
                                bundle.traces.mark_active(&key, new_id);
                                if let Some(e) = bundle.traces.get_mut(&key) {
                                    e.invocations.clear();
                                }
                            }
                        });
                    }
                    Err(e) => {
                        failed += 1;
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.traces.mark_failed(&key, e.clone());
                            }
                            shell.push_dock_log(
                                format!("✗ trace {}.{}: {e}",
                                    key.class_jni, key.method_name),
                                cx,
                            );
                        });
                    }
                }
            }
            for entry in &hooks {
                let new_id = session.alloc_script_id();
                let body = match &entry.action {
                    crate::hooks::HookAction::LogOnly => {
                        glass_frida::HookBody::LogOnly
                    }
                    crate::hooks::HookAction::ReturnLiteral(lit) => {
                        glass_frida::HookBody::ReturnLiteral(lit.clone())
                    }
                    crate::hooks::HookAction::CustomJs(body) => {
                        glass_frida::HookBody::Custom(body.clone())
                    }
                };
                let js = match glass_frida::render_hook_script(
                    &entry.key.class_jni,
                    &entry.key.method_name,
                    &entry.key.method_signature,
                    &body,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        failed += 1;
                        let key = entry.key.clone();
                        let err = format!("{e}");
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.hooks.mark_failed(&key, err.clone());
                            }
                            shell.push_dock_log(
                                format!("✗ hook render {}.{}: {err}",
                                    key.class_jni, key.method_name),
                                cx,
                            );
                        });
                        continue;
                    }
                };
                let name = format!(
                    "hook-{}-{}",
                    entry.key.class_jni.replace('/', "."),
                    entry.key.method_name
                );
                let key = entry.key.clone();
                let res = cx
                    .background_executor()
                    .spawn({
                        let session = session.clone();
                        async move { session.create_script(new_id, name, js) }
                    })
                    .await;
                match res {
                    Ok(()) => {
                        installed += 1;
                        let _ = this.update(cx, |shell, _cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.hooks.mark_active(&key, new_id);
                                if let Some(e) = bundle.hooks.get_mut(&key) {
                                    e.invocations.clear();
                                }
                            }
                        });
                    }
                    Err(e) => {
                        failed += 1;
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.hooks.mark_failed(&key, e.clone());
                            }
                            shell.push_dock_log(
                                format!("✗ hook {}.{}: {e}",
                                    key.class_jni, key.method_name),
                                cx,
                            );
                        });
                    }
                }
            }
            let total = traces.len() + hooks.len();
            if total > 0 {
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(
                        format!("• installed {installed}/{total} ({failed} failed)"),
                        cx,
                    );
                });
            }
            // 7. resume — gadget unblocks, app starts running.
            let resume_res = cx
                .background_executor()
                .spawn({
                    let session = session.clone();
                    async move { session.resume(pid) }
                })
                .await;
            let _ = this.update(cx, |shell, cx| match resume_res {
                Ok(()) => {
                    shell.push_dock_log("▶ resumed — app running", cx);
                }
                Err(e) => {
                    shell.push_dock_log(format!("✗ resume: {e}"), cx);
                }
            });
        })
        .detach();
    }

    /// Force-stop the dock's package on the dock's device.
    pub(crate) fn debug_stop(&mut self, cx: &mut Context<Self>) {
        let Some(dock) = self.debug_dock.as_ref() else { return };
        let serial = dock.device.id.serial.clone();
        let package = dock.package.clone();
        let device_manager = self.device_manager.clone();
        self.push_dock_log(format!("◼ stopping {package}"), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let status = device_manager.backend_status().adb.clone();
                    let backend = match status {
                        Ok(info) => {
                            glass_device::adb::AdbBackend::with_override(
                                Some(info.binary_path),
                            )
                        }
                        Err(e) => Err(glass_device::DeviceError::Backend(
                            format!("{e}"),
                        )),
                    };
                    match backend {
                        Ok(b) => b.force_stop(&serial, &package),
                        Err(e) => Err(e),
                    }
                })
                .await;
            let line = match result {
                Ok(s) if s.trim().is_empty() => "(stopped)".to_string(),
                Ok(s) => s,
                Err(e) => format!("error: {e}"),
            };
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log(line, cx);
            });
        })
        .detach();
    }

    pub(crate) fn toggle_traces_dialog(&mut self, cx: &mut Context<Self>) {
        self.traces_dialog_open = !self.traces_dialog_open;
        cx.notify();
    }

    pub(crate) fn close_traces_dialog(&mut self, cx: &mut Context<Self>) {
        if self.traces_dialog_open {
            self.traces_dialog_open = false;
            cx.notify();
        }
    }

    /// Stop every active trace. Used by the "Stop all" footer
    /// in the trace dialog. Iterates the registry, drains
    /// keys (so we don't double-borrow), unloads each script.
    pub(crate) fn stop_all_traces(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<crate::traces::TraceKey> = self
            .bundle()
            .map(|b| b.traces.entries().iter().map(|e| e.key.clone()).collect())
            .unwrap_or_default();
        for k in keys {
            self.stop_trace(
                k.artifact,
                k.class_jni,
                k.method_name,
                k.method_signature,
                cx,
            );
        }
    }
}
