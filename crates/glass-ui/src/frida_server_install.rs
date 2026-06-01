//! "Push frida-server to this rooted device" flow.
//!
//! Triggered from the device-picker dropdown when the Frida
//! probe reports `ServerUnreachable` on an Android device.
//! Mirrors the gadget-injection executor in `frida_inject.rs`:
//! state lives on `Shell`, work runs on the background
//! executor, progress is posted back through `cx.update_entity`.
//!
//! Stages:
//!   1. Read `getprop ro.product.cpu.abi` to pick the arch.
//!   2. Stage the matching `frida-server-<ver>-android-<arch>`
//!      from `glass_frida::server` (download + xz decompress,
//!      cached under `~/Library/Caches/glass/frida-server/`).
//!   3. `adb push` to `/data/local/tmp/frida-server`.
//!   4. `adb shell su -c 'chmod 755 …'`.
//!   5. `adb shell su -c 'nohup … &'`.
//!   6. Drop the cached probe so the picker re-runs it and the
//!      chip flips to "frida-server <ver>".

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};

use crate::Shell;

/// What step the install is on. Drives the headline in the
/// progress overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FridaServerInstallPhase {
    DetectingAbi,
    Staging,
    Pushing,
    Starting,
    Done,
}

impl FridaServerInstallPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::DetectingAbi => "Detecting device CPU ABI…",
            Self::Staging => "Downloading & extracting frida-server…",
            Self::Pushing => "Pushing to /data/local/tmp…",
            Self::Starting => "Starting frida-server (su)…",
            Self::Done => "Done.",
        }
    }
}

/// Shell-side state for an in-flight or just-finished install.
#[derive(Clone, Debug)]
pub(crate) struct FridaServerInstallProgress {
    pub phase: FridaServerInstallPhase,
    /// Combined progress + adb output. Rendered as a small log
    /// panel under the phase headline.
    pub log: Vec<String>,
    /// `None` while running, `Some(Ok(remote_path))` when the
    /// device-side `nohup …` returned, or `Some(Err(msg))` on
    /// any failure along the way.
    pub result: Option<Result<String, String>>,
}

impl FridaServerInstallProgress {
    fn running(phase: FridaServerInstallPhase, first_log: String) -> Self {
        Self { phase, log: vec![first_log], result: None }
    }
}

/// Remote path we install to. `/data/local/tmp` is world-
/// writable on every Android build; `su` runs the binary from
/// there with root uid.
const REMOTE_PATH: &str = "/data/local/tmp/frida-server";

impl Shell {
    /// Kick off the install flow for `device`. Idempotent — if
    /// a previous run is still in flight we no-op rather than
    /// stacking jobs.
    pub(crate) fn execute_frida_server_install(
        &mut self,
        device: glass_device::DeviceInfo,
        cx: &mut Context<Self>,
    ) {
        if self.frida_server_install.is_some() {
            return;
        }
        let serial = device.id.serial.clone();
        let device_manager = self.device_manager.clone();
        self.frida_server_install = Some(FridaServerInstallProgress::running(
            FridaServerInstallPhase::DetectingAbi,
            format!("adb -s {serial} shell getprop ro.product.cpu.abi"),
        ));
        cx.notify();

        cx.spawn(async move |this, cx| {
            // Phase 1: ABI.
            let serial_for_abi = serial.clone();
            let dm = device_manager.clone();
            let abi_result: Result<String, String> = cx
                .background_executor()
                .spawn(async move {
                    let backend = open_adb(&dm)?;
                    backend
                        .primary_abi(&serial_for_abi)
                        .map_err(|e| format!("{e}"))
                })
                .await;
            let abi = match abi_result {
                Ok(abi) => abi,
                Err(e) => {
                    fail(this, cx, &format!("Reading ABI failed: {e}")).await;
                    return;
                }
            };
            let arch = match glass_frida::AndroidServerArch::from_abi(&abi) {
                Some(a) => a,
                None => {
                    fail(
                        this,
                        cx,
                        &format!(
                            "No published frida-server for device ABI {abi:?}"
                        ),
                    )
                    .await;
                    return;
                }
            };
            let _ = this.update(cx, |shell, cx| {
                if let Some(p) = shell.frida_server_install.as_mut() {
                    p.phase = FridaServerInstallPhase::Staging;
                    p.log.push(format!("Device ABI: {abi} → {}", arch.slug()));
                    p.log.push(format!(
                        "frida-server {} ({})",
                        glass_frida::FRIDA_VERSION,
                        glass_frida::frida_server_asset_url(arch),
                    ));
                }
                cx.notify();
            });

            // Phase 2: stage (download + xz decompress).
            // The progress callback fires on every 64KB chunk;
            // we throttle log appends so we don't flood the
            // panel — only emit a line on multiples of 1 MiB
            // and on the final byte.
            let stage_result: Result<std::path::PathBuf, glass_frida::ServerStageError> = cx
                .background_executor()
                .spawn(async move {
                    let mut last_bucket: u64 = 0;
                    glass_frida::stage_server(arch, |p| {
                        match p {
                            glass_frida::StageProgress::CacheHit => {
                                tracing::info!("frida-server cache hit");
                            }
                            glass_frida::StageProgress::Downloading {
                                bytes_so_far,
                                total,
                            } => {
                                let bucket = bytes_so_far / (1024 * 1024);
                                if bucket != last_bucket {
                                    last_bucket = bucket;
                                    tracing::debug!(
                                        bytes_so_far,
                                        ?total,
                                        "downloading frida-server"
                                    );
                                }
                            }
                            _ => {}
                        }
                    })
                })
                .await;
            let local_path = match stage_result {
                Ok(p) => p,
                Err(e) => {
                    fail(this, cx, &format!("Staging frida-server failed: {e}"))
                        .await;
                    return;
                }
            };
            let _ = this.update(cx, |shell, cx| {
                if let Some(p) = shell.frida_server_install.as_mut() {
                    p.phase = FridaServerInstallPhase::Pushing;
                    p.log.push(format!("Staged: {}", local_path.display()));
                    p.log.push(format!(
                        "adb -s {serial} push <…> {REMOTE_PATH}",
                    ));
                }
                cx.notify();
            });

            // Phase 3: push + chmod.
            let serial_for_push = serial.clone();
            let dm = device_manager.clone();
            let local_for_push = local_path.clone();
            let push_result: Result<String, String> = cx
                .background_executor()
                .spawn(async move {
                    let backend = open_adb(&dm)?;
                    let push_log = backend
                        .push(&serial_for_push, &local_for_push, REMOTE_PATH)
                        .map_err(|e| format!("{e}"))?;
                    // chmod via `su` so it works even when the
                    // shell user can't touch /data/local/tmp's
                    // existing entry (rare but possible on some
                    // builds).
                    let chmod_log = backend
                        .shell(
                            &serial_for_push,
                            &[
                                "su",
                                "-c",
                                &format!("chmod 755 {REMOTE_PATH}"),
                            ],
                        )
                        .map_err(|e| format!("{e}"))?;
                    Ok(format!("{push_log}\n{chmod_log}"))
                })
                .await;
            match push_result {
                Ok(out) => {
                    let _ = this.update(cx, |shell, cx| {
                        if let Some(p) = shell.frida_server_install.as_mut() {
                            for line in out.lines() {
                                let line = line.trim_end();
                                if !line.is_empty() {
                                    p.log.push(line.to_string());
                                }
                            }
                        }
                        cx.notify();
                    });
                }
                Err(e) => {
                    fail(this, cx, &format!("Push failed: {e}")).await;
                    return;
                }
            }

            // Phase 4: start (su -c nohup …).
            let _ = this.update(cx, |shell, cx| {
                if let Some(p) = shell.frida_server_install.as_mut() {
                    p.phase = FridaServerInstallPhase::Starting;
                    p.log.push(format!(
                        "adb -s {serial} shell \"su -c 'nohup {REMOTE_PATH} &'\"",
                    ));
                }
                cx.notify();
            });
            let serial_for_start = serial.clone();
            let dm = device_manager.clone();
            let start_result: Result<String, String> = cx
                .background_executor()
                .spawn(async move {
                    let backend = open_adb(&dm)?;
                    backend
                        .start_frida_server(&serial_for_start, REMOTE_PATH)
                        .map_err(|e| format!("{e}"))
                })
                .await;

            // Finalise either way — Done with success or with
            // the start error. The probe will confirm reality.
            let _ = this.update(cx, |shell, cx| {
                let mut p = shell
                    .frida_server_install
                    .take()
                    .unwrap_or_else(|| FridaServerInstallProgress {
                        phase: FridaServerInstallPhase::Done,
                        log: Vec::new(),
                        result: None,
                    });
                p.phase = FridaServerInstallPhase::Done;
                match start_result {
                    Ok(out) => {
                        let trimmed = out.trim();
                        if !trimmed.is_empty() {
                            p.log.push(trimmed.to_string());
                        }
                        p.log.push(
                            "Started. Re-probing device to confirm…".into(),
                        );
                        p.result = Some(Ok(REMOTE_PATH.to_string()));
                    }
                    Err(e) => {
                        p.log.push(format!("Start failed: {e}"));
                        p.result = Some(Err(e));
                    }
                }
                shell.frida_server_install = Some(p);
                // Force the picker to re-probe — the chip
                // should flip to "frida-server <ver>" once the
                // background poll tick runs.
                if let Some(id) = shell.selected_device.as_ref() {
                    shell.frida_probes.remove(id);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Clear the install overlay after the user clicks Dismiss.
    pub(crate) fn dismiss_frida_server_install(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.frida_server_install.take().is_some() {
            cx.notify();
        }
    }
}

/// Helper: resolve a working `AdbBackend` from the device
/// manager's cached status. Returns a stringified error so
/// callers can shove it straight onto the log.
fn open_adb(
    dm: &std::sync::Arc<glass_device::DeviceManager>,
) -> Result<glass_device::adb::AdbBackend, String> {
    let status = dm.backend_status();
    let adb = status
        .adb
        .map_err(|e| format!("adb backend unavailable: {e}"))?;
    glass_device::adb::AdbBackend::with_override(Some(adb.binary_path))
        .map_err(|e| format!("opening adb: {e}"))
}

/// Drop the install state into a `Done(Err(msg))` so the dialog
/// renders the failure and the user can dismiss it.
async fn fail(
    this: gpui::WeakEntity<Shell>,
    cx: &mut gpui::AsyncApp,
    msg: &str,
) {
    let msg = msg.to_string();
    let _ = this.update(cx, |shell, cx| {
        shell.frida_server_install = Some(FridaServerInstallProgress {
            phase: FridaServerInstallPhase::Done,
            log: vec![msg.clone()],
            result: Some(Err(msg)),
        });
        cx.notify();
    });
}

/// Render the modal overlay for the install. Same shape as
/// `injection_dialog::render_injection_progress` — 720px panel,
/// phase headline, scrolling log, Dismiss button when finished.
pub fn render_frida_server_install(
    progress: &FridaServerInstallProgress,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let running = progress.result.is_none();
    let theme = crate::theme::current();
    let phase_color = match (&progress.result, progress.phase) {
        (Some(Err(_)), _) => theme.errors.highlight.rgba(),
        (_, FridaServerInstallPhase::Done) => accent,
        _ => fg,
    };

    let mut log_col = div().flex().flex_col().gap_0p5();
    for line in &progress.log {
        log_col = log_col.child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(SharedString::from(line.clone())),
        );
    }

    let mut card = div()
        .id("frida-server-install-card")
        .w(px(720.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_md()
        .shadow_lg()
        .p_5()
        .flex()
        .flex_col()
        .gap_3()
        .occlude()
        .child(
            div()
                .text_lg()
                .text_color(phase_color)
                .child(SharedString::from(progress.phase.label())),
        )
        .child(log_col)
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        );

    if !running {
        let dismiss = div()
            .id("frida-server-install-dismiss")
            .px_3()
            .py_1p5()
            .border_1()
            .border_color(accent)
            .rounded_sm()
            .text_sm()
            .text_color(fg)
            .cursor_pointer()
            .child(SharedString::from("Dismiss"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|shell, _ev, _w, cx| {
                    shell.dismiss_frida_server_install(cx);
                }),
            );
        card = card.child(
            div().flex().flex_row().justify_end().child(dismiss),
        );
    }

    div()
        .absolute()
        .inset_0()
        .bg(theme.modals.overlay_light.rgba())
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}
