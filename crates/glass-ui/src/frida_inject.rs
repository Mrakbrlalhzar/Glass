//! Frida gadget injection.
//!
//! Lifted out of `shell_actions.rs` so the file stays under the
//! module-size discipline. The methods are still defined on
//! `Shell` via a sibling `impl Shell` block — Rust allows
//! multiple `impl Shell` blocks across files in the same crate,
//! so the existing call sites continue to work without renames.

use gpui::Context;

use crate::Shell;

impl Shell {
    // ---- Frida gadget injection ---------------------------------------

    /// Open the gadget-injection dialog for the currently-loaded
    /// bundle. Computes an `InjectionPlan` synchronously (it's
    /// pure inspection) and stashes the result on Shell so the
    /// dialog renderer doesn't have to rebuild it every frame.
    ///
    /// Returns `false` and no-ops when:
    ///   * No bundle is loaded.
    ///   * The bundle isn't an APK (no AndroidManifest).
    ///   * The bundle's manifest failed to decode.
    /// In each case we log a hint and leave the picker dropdown
    /// open so the user can see why nothing happened.
    pub(crate) fn open_injection_dialog(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(bundle) = self.bundle() else {
            tracing::info!("open_injection_dialog: no bundle loaded");
            return false;
        };
        let Some(manifest) = bundle.android_manifest.as_ref() else {
            tracing::info!(
                "open_injection_dialog: bundle has no AndroidManifest — \
                 gadget injection is APK-only for now"
            );
            return false;
        };
        // Collect inputs for the planner. `loaded_classes` is
        // the set of JNI sigs we've lifted smali for; the
        // planner uses it to warn when the manifest references
        // a class we don't actually have.
        let loaded_classes: std::collections::BTreeSet<String> = bundle
            .smali_classes
            .keys()
            .map(|(_, jni)| jni.clone())
            .collect();
        // ABIs the APK carries native libs for — `lib/<abi>/`.
        // We read this from the existing `origins` field where
        // each native-lib leaf records its `lib/<abi>/<name>`
        // string.
        let mut native_abis: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        let mut abis_with_gadget: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for (i, origin) in bundle.origins.iter().enumerate() {
            // origins like "lib/arm64-v8a" (set by the loader
            // for native-lib leaves). Strip the prefix to get
            // the ABI string.
            let s = origin.as_ref();
            if let Some(abi) = s.strip_prefix("lib/") {
                native_abis.insert(abi.to_string());
                // If a leaf labelled libfrida-gadget.so sits in
                // this ABI directory, flag the warning. The
                // leaf's label is the bare filename.
                if let Some(label) = bundle.labels.get(i) {
                    if label.as_ref() == "libfrida-gadget.so" {
                        abis_with_gadget.insert(abi.to_string());
                    }
                }
            }
        }
        let inputs = glass_frida::PlanInputs {
            manifest: Some(&**manifest),
            loaded_classes,
            native_abis,
            abis_with_gadget,
        };
        let plan = match glass_frida::plan_injection(&inputs) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(?e, "open_injection_dialog: planner refused");
                return false;
            }
        };
        // Capture which device the user has currently selected
        // so the dialog can offer "Inject & install on …" even
        // if the chip selection changes while the dialog is
        // open.
        let target_device = self
            .selected_device
            .as_ref()
            .and_then(|id| {
                self.device_snapshot.iter().find(|d| &d.id == id).cloned()
            });
        self.injection_dialog = Some(crate::InjectionDialogState {
            plan,
            target_device,
        });
        // Close the picker dropdown so the dialog is the only
        // overlay competing for attention.
        self.device_picker_open = false;
        cx.notify();
        true
    }

    pub(crate) fn close_injection_dialog(&mut self, cx: &mut Context<Self>) {
        if self.injection_dialog.take().is_some() {
            cx.notify();
        }
    }

    /// Apply the gadget-injection plan to the loaded bundle.
    /// Stages a smali edit on the patch-target class (visible
    /// in the Changes dialog like any other smali edit) and
    /// registers `lib/<abi>/libfrida-gadget.so` as a pending
    /// APK addition for every supported ABI in the plan.
    ///
    /// After this the user clicks the toolbar's existing
    /// "Export N changes…" button to write the patched APK.
    /// Sign + install are M3.2d/e.
    pub(crate) fn execute_injection(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.injection_dialog.as_ref() else { return };
        // Pluck the JNI of the patch-target class so we can
        // locate it in the bundle.
        let target_jni = match &state.plan.patch_target {
            glass_frida::PatchTarget::ExistingApplication { class_jni, .. } => {
                class_jni.clone()
            }
            glass_frida::PatchTarget::LauncherActivity { class_jni, .. } => {
                class_jni.clone()
            }
            glass_frida::PatchTarget::SynthesiseRequired => {
                tracing::warn!(
                    "execute_injection: plan needs class synthesis (not implemented)"
                );
                self.close_injection_dialog(cx);
                return;
            }
        };
        let plan = state.plan.clone();
        // Find the artifact (DEX) that contains this class.
        // smali_classes is keyed by (artifact_id, class_jni)
        // so we can lift the class out and learn its DEX in
        // one pass.
        let (artifact_id, base_class) = {
            let Some(bundle) = self.bundle() else {
                self.close_injection_dialog(cx);
                return;
            };
            let hit = bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
                if jni == &target_jni {
                    Some((aid.clone(), c.clone()))
                } else {
                    None
                }
            });
            match hit {
                Some(x) => x,
                None => {
                    tracing::warn!(
                        target_jni = %target_jni,
                        "execute_injection: class isn't in the lifted set"
                    );
                    self.close_injection_dialog(cx);
                    return;
                }
            }
        };
        // Layer on top of any earlier staged edit for the same
        // class so the gadget patch coexists with whatever the
        // user might have changed manually.
        let starting_class = self
            .bundle()
            .and_then(|b| b.smali_edits.get(&artifact_id, &target_jni))
            .map(|e| e.modified.clone())
            .unwrap_or(base_class);
        let patched = match glass_frida::apply_plan(&starting_class, &plan) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(?e, "execute_injection: apply_plan failed");
                self.close_injection_dialog(cx);
                return;
            }
        };
        // Stage the modified class through the existing path
        // so it shows up in the Changes dialog like any other
        // smali edit (revertable, line-cached invalidation,
        // tinting on the smali tab).
        self.stage_smali_class_edit(artifact_id, target_jni, patched, cx);
        // Add the gadget binary to the bundle's pending APK
        // additions for every ABI we ship a gadget for.
        // Today that's arm64-v8a only; other ABIs in the plan
        // are skipped with a log line so the user can see what
        // didn't make it.
        let mut added_abis: Vec<String> = Vec::new();
        let mut skipped_abis: Vec<String> = Vec::new();
        // Every gadget binary needs its config sibling — recent
        // gadget releases (17.x) refuse to load without
        // libfrida-gadget.config.so next to them. Stage the
        // listen-mode config alongside every .so we add.
        let config_filename = glass_frida::ANDROID_GADGET_CONFIG_FILENAME;
        let config_bytes = glass_frida::android_gadget_config_listen();
        for abi in &plan.abis {
            match glass_frida::for_android_abi(abi) {
                Some(gadget) => {
                    if let Some(bundle) = self.bundle_mut() {
                        let zip_path = format!("lib/{abi}/{}", gadget.filename);
                        bundle
                            .pending_additions
                            .insert(zip_path, gadget.bytes.to_vec());
                        let cfg_path = format!("lib/{abi}/{config_filename}");
                        bundle
                            .pending_additions
                            .insert(cfg_path, config_bytes.clone());
                    }
                    added_abis.push(abi.clone());
                }
                None => skipped_abis.push(abi.clone()),
            }
        }
        // If no ABI matched, also drop the gadget under
        // arm64-v8a regardless — Android will pick it up on
        // arm64 phones even if the APK didn't ship any other
        // arm64 libs. (Devices choose libs by ABI; an APK with
        // only x86 libs but with arm64-v8a frida-gadget will
        // load it correctly on a Pixel.)
        if added_abis.is_empty() {
            if let Some(gadget) = glass_frida::for_android_abi("arm64-v8a") {
                if let Some(bundle) = self.bundle_mut() {
                    bundle.pending_additions.insert(
                        format!("lib/arm64-v8a/{}", gadget.filename),
                        gadget.bytes.to_vec(),
                    );
                    bundle.pending_additions.insert(
                        format!("lib/arm64-v8a/{config_filename}"),
                        config_bytes.clone(),
                    );
                }
                added_abis.push("arm64-v8a".to_string());
            }
        }
        tracing::info!(
            added = ?added_abis,
            skipped = ?skipped_abis,
            "gadget bytes registered as pending APK additions",
        );
        self.close_injection_dialog(cx);
    }

    /// Full "Inject & Install" pipeline. Stages the smali edit
    /// + gadget addition (same as `execute_injection`), then on
    /// a background task: writes a temp APK via the existing
    /// export pipeline, signs it with the Glass debug keystore,
    /// and `adb install -r`s it on the target device. Progress
    /// is reported on `Shell.injection_progress` so the GUI
    /// can show a streaming status overlay.
    pub(crate) fn execute_injection_and_install(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        // First stage the changes via the existing path so the
        // Changes dialog still shows what got patched.
        let Some(state) = self.injection_dialog.clone() else { return };
        let Some(target) = state.target_device.clone() else {
            tracing::warn!("execute_injection_and_install: no device selected");
            return;
        };
        if !matches!(target.state, glass_device::AuthState::Authorised) {
            self.injection_progress = Some(crate::InjectionProgress {
                phase: crate::InjectionPhase::Done,
                log: vec![format!(
                    "Device {} isn't authorised — accept the USB-debug prompt on it first.",
                    target.id.serial,
                )],
                result: Some(Err("device unauthorised".into())),
            });
            self.close_injection_dialog(cx);
            cx.notify();
            return;
        }
        if !matches!(target.id.platform, glass_device::DevicePlatform::Android) {
            self.injection_progress = Some(crate::InjectionProgress {
                phase: crate::InjectionPhase::Done,
                log: vec![format!(
                    "Selected device {} is iOS — inject-and-install is Android-only for now.",
                    target.id.serial,
                )],
                result: Some(Err("ios install path not implemented".into())),
            });
            self.close_injection_dialog(cx);
            cx.notify();
            return;
        }
        // Discover sign tools *before* any disk writes. If
        // they're missing the user sees a clean error rather
        // than a half-baked patched APK on disk.
        let signer = match glass_frida::SignerTools::discover() {
            Ok(s) => s,
            Err(e) => {
                self.injection_progress = Some(crate::InjectionProgress {
                    phase: crate::InjectionPhase::Done,
                    log: vec![format!("{e}")],
                    result: Some(Err(format!("sign tools missing: {e}"))),
                });
                self.close_injection_dialog(cx);
                cx.notify();
                return;
            }
        };
        // Reuse `execute_injection` to stage the smali edit +
        // gadget addition. This closes the dialog as a
        // side-effect (it always does); we don't need to call
        // close again.
        self.execute_injection(cx);
        // Build the inputs for the background task.
        let source_path = self
            .source_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let stem = source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("patched");
        let temp_dir = std::env::temp_dir().join("glass-inject");
        if let Err(e) = std::fs::create_dir_all(&temp_dir) {
            self.injection_progress = Some(crate::InjectionProgress {
                phase: crate::InjectionPhase::Done,
                log: vec![format!("creating {}: {e}", temp_dir.display())],
                result: Some(Err(format!("tempdir: {e}"))),
            });
            cx.notify();
            return;
        }
        let out_path = temp_dir.join(format!("{stem}-frida.apk"));
        // Snapshot everything the executor needs so the
        // background task doesn't hold a borrow on Shell.
        let Some(bundle) = self.bundle() else { return };
        let mut edit_map: std::collections::HashMap<
            glass_db::ArtifactId,
            Vec<glass_api::EditPatch>,
        > = std::collections::HashMap::new();
        for e in bundle.edits.entries() {
            edit_map
                .entry(e.artifact.clone())
                .or_default()
                .push(glass_api::EditPatch {
                    vaddr: e.vaddr,
                    new_bytes: e.new_bytes.clone(),
                });
        }
        let mut smali_edit_map: glass_api::SmaliEditMap =
            std::collections::HashMap::new();
        for e in bundle.smali_edits.entries() {
            smali_edit_map
                .entry(e.key.artifact.clone())
                .or_default()
                .insert(e.key.class_jni.clone(), e.modified.clone());
        }
        let additions: glass_api::ApkAdditions = bundle.pending_additions.clone();
        let serial = target.id.serial.clone();
        let device_manager = self.device_manager.clone();
        // Initial progress state.
        self.injection_progress = Some(crate::InjectionProgress {
            phase: crate::InjectionPhase::Exporting,
            log: vec![format!("Writing patched APK to {}", out_path.display())],
            result: None,
        });
        cx.notify();
        // Spawn the pipeline.
        cx.spawn(async move |this, cx| {
            // Phase 1: export.
            let export_result: Result<(), String> = cx
                .background_executor()
                .spawn({
                    let source_path = source_path.clone();
                    let out_path = out_path.clone();
                    async move {
                        match glass_api::open(&source_path) {
                            Ok(bundle) => glass_api::export_to_path_with_smali(
                                &bundle,
                                &edit_map,
                                &smali_edit_map,
                                &additions,
                                &out_path,
                            )
                            .map_err(|e| format!("{e:#}")),
                            Err(e) => Err(format!("re-open failed: {e:#}")),
                        }
                    }
                })
                .await;
            if let Err(e) = export_result {
                let _ = this.update(cx, |shell, cx| {
                    let log = vec![format!("Export failed: {e}")];
                    shell.injection_progress = Some(crate::InjectionProgress {
                        phase: crate::InjectionPhase::Done,
                        log,
                        result: Some(Err(e)),
                    });
                    cx.notify();
                });
                return;
            }
            // Phase 2: sign.
            let _ = this.update(cx, |shell, cx| {
                if let Some(p) = shell.injection_progress.as_mut() {
                    p.phase = crate::InjectionPhase::Signing;
                    p.log.push(format!(
                        "Signing with {}",
                        signer.keystore_path.display()
                    ));
                }
                cx.notify();
            });
            let signer_for_task = signer.clone();
            let out_path_for_task = out_path.clone();
            let sign_result: Result<String, glass_frida::SignError> = cx
                .background_executor()
                .spawn(async move {
                    signer_for_task.ensure_keystore()?;
                    signer_for_task.sign(&out_path_for_task)
                })
                .await;
            match sign_result {
                Ok(stdout) => {
                    let _ = this.update(cx, |shell, cx| {
                        if let Some(p) = shell.injection_progress.as_mut() {
                            if !stdout.trim().is_empty() {
                                p.log.push(stdout.trim().to_string());
                            }
                        }
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |shell, cx| {
                        shell.injection_progress = Some(crate::InjectionProgress {
                            phase: crate::InjectionPhase::Done,
                            log: vec![format!("Sign failed: {e}")],
                            result: Some(Err(format!("{e}"))),
                        });
                        cx.notify();
                    });
                    return;
                }
            }
            // Phase 3: adb install.
            let _ = this.update(cx, |shell, cx| {
                if let Some(p) = shell.injection_progress.as_mut() {
                    p.phase = crate::InjectionPhase::Installing;
                    p.log.push(format!("adb -s {serial} install -r"));
                }
                cx.notify();
            });
            let serial_for_task = serial.clone();
            let out_for_task = out_path.clone();
            let install_result: Result<String, glass_device::DeviceError> = cx
                .background_executor()
                .spawn(async move {
                    let status = device_manager.backend_status();
                    let adb = status
                        .adb
                        .map_err(|e| glass_device::DeviceError::Backend(format!("adb: {e}")))?;
                    // We need a fresh AdbBackend here — backend_status
                    // returned info, but the install verb lives on
                    // the backend itself. Re-discover the binary.
                    let backend = glass_device::adb::AdbBackend::with_override(
                        Some(adb.binary_path),
                    )
                    .map_err(|e| glass_device::DeviceError::Backend(format!("{e}")))?;
                    backend.install(&serial_for_task, &out_for_task)
                })
                .await;
            let _ = this.update(cx, |shell, cx| {
                let mut p = shell
                    .injection_progress
                    .take()
                    .unwrap_or_else(|| crate::InjectionProgress {
                        phase: crate::InjectionPhase::Done,
                        log: Vec::new(),
                        result: None,
                    });
                p.phase = crate::InjectionPhase::Done;
                match install_result {
                    Ok(stdout) => {
                        if !stdout.trim().is_empty() {
                            p.log.push(stdout.trim().to_string());
                        }
                        p.result = Some(Ok(out_path.clone()));
                    }
                    Err(e) => {
                        p.log.push(format!("Install failed: {e}"));
                        p.result = Some(Err(format!("{e}")));
                    }
                }
                shell.injection_progress = Some(p);
                // We just changed the device's state — the
                // newly-installed app probably hasn't launched
                // yet so the chip should re-probe and reflect
                // current reality, not the cached "yes Frida"
                // from before we ran.
                if let Some(id) = shell.selected_device.as_ref() {
                    shell.frida_probes.remove(id);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Copy the current dock log to the system clipboard.
    /// Workaround for gpui's lack of native text selection
    /// in the dock — the user can now grab full error
    /// messages instead of squinting.
    pub(crate) fn copy_debug_dock_log(&mut self, cx: &mut Context<Self>) {
        let Some(dock) = self.debug_dock.as_ref() else { return };
        let joined = dock.log.join("\n");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(joined));
        // Tiny confirmation so the user knows it landed —
        // appending to the dock log instead of a toast keeps
        // the noise local.
        self.push_dock_log("(log copied to clipboard)", cx);
    }

    pub(crate) fn dismiss_injection_progress(&mut self, cx: &mut Context<Self>) {
        if self.injection_progress.take().is_some() {
            cx.notify();
        }
    }
}
