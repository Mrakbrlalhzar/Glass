//! Coverage-recording lifecycle wired into Shell.
//!
//! `start_coverage_recording` is the user-driven entry point —
//! validates that a Frida session exists, builds the Stalker
//! coverage script via the shared `glass_frida::render_coverage_script`,
//! allocates a script id, kicks off the load on a background
//! executor, and parks the new id on `coverage_recording` so
//! the global session-event pump (see `app::route_session_event`)
//! can route the result back via `route_coverage_event`.
//!
//! Why use the global pump rather than a per-recording loop:
//! every Frida event flows through one channel; spinning a
//! dedicated `poll_events` loop would race the existing pump
//! and steal unrelated messages. Routing by `script_id` keeps
//! the recording state machine cleanly separated.

use std::sync::Arc;

use gpui::Context;

use crate::coverage_view::{
    CoverageRecording, CoverageRecordingState,
};
use crate::Shell;

impl Shell {
    /// Begin a coverage recording for `duration_ms`. No-op when
    /// no Frida session is attached. Sets `coverage_recording`
    /// to `Recording`; the pump will flip it to `Loaded` once
    /// the script's final `send` arrives.
    pub(crate) fn start_coverage_recording(
        &mut self,
        duration_ms: u64,
        cx: &mut Context<Self>,
    ) {
        let session = self
            .debug_dock
            .as_ref()
            .and_then(|d| d.session.clone());
        let Some(session) = session else {
            self.coverage_recording = CoverageRecordingState::Failed(
                "no Frida session attached — open the device dock and \
                 connect first"
                    .to_string(),
            );
            cx.notify();
            return;
        };

        // Clamp duration. Anything under 100ms gives no useful
        // signal; anything over 30s blocks the UI feedback loop
        // for too long without a stop button (which we'll add
        // when diff-mode lands).
        let duration_ms = duration_ms.clamp(100, 30_000);

        // Build the Stalker JS — same template the MCP server
        // uses. Empty `modules` ⇒ filter to *all* loaded modules
        // (we'll keep what we can resolve via the bundle).
        // Empty `tids` ⇒ follow the target's main thread.
        let source =
            glass_frida::render_coverage_script(&[], &[], duration_ms);
        let name = "glass-ui-coverage";
        let script_id = session.alloc_script_id();
        self.coverage_recording = CoverageRecordingState::Recording {
            script_id,
            started_at: std::time::Instant::now(),
            duration_ms,
        };
        // Save the duration so the next click defaults to the
        // same value.
        self.coverage_duration_ms = duration_ms;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn({
                    let session = session.clone();
                    async move {
                        session.create_script(script_id, name, source)
                    }
                })
                .await;
            if let Err(e) = res {
                let _ = this.update(cx, |shell, cx| {
                    shell.coverage_recording =
                        CoverageRecordingState::Failed(format!(
                            "create_script: {e}"
                        ));
                    cx.notify();
                });
            }
            // No further polling here: the global pump in
            // `app::spawn_debug_dock_pump` drains events and
            // hands the matching script's payload to
            // `route_coverage_event_or_pass`.
        })
        .detach();
    }

    /// Called by the global event router. Returns `None` when
    /// the event belonged to the active coverage recording
    /// (and was handled), or `Some(ev)` when it should be
    /// passed through to the other routers.
    pub(crate) fn route_coverage_event_or_pass(
        &mut self,
        ev: glass_frida::SessionEvent,
    ) -> Option<glass_frida::SessionEvent> {
        let active_id = match &self.coverage_recording {
            CoverageRecordingState::Recording { script_id, .. } => *script_id,
            _ => return Some(ev),
        };
        // Match-by-script-id without consuming `ev` first, so
        // we can hand it back when it isn't ours.
        let our_event = match &ev {
            glass_frida::SessionEvent::ScriptMessage { script_id, .. }
            | glass_frida::SessionEvent::ScriptError { script_id, .. }
                if *script_id == active_id =>
            {
                true
            }
            _ => false,
        };
        if !our_event {
            return Some(ev);
        }

        match ev {
            glass_frida::SessionEvent::ScriptMessage { payload, .. } => {
                match payload.get("kind").and_then(|v| v.as_str()) {
                    Some("stalker-coverage-init") => {
                        // Init message arrives moments after the
                        // script loads. Swallow it to keep it
                        // out of the dock log.
                        None
                    }
                    Some("stalker-coverage") => {
                        let duration_ms = match &self.coverage_recording {
                            CoverageRecordingState::Recording {
                                duration_ms,
                                ..
                            } => *duration_ms,
                            _ => 0,
                        };
                        let rows: Vec<serde_json::Value> = payload
                            .get("rows")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let recording = match self.bundle() {
                            Some(b) => CoverageRecording::from_rows(
                                duration_ms, &rows, b,
                            ),
                            None => {
                                self.coverage_recording =
                                    CoverageRecordingState::Failed(
                                        "no bundle loaded".into(),
                                    );
                                return None;
                            }
                        };
                        self.coverage_recording =
                            CoverageRecordingState::Loaded(Arc::new(
                                recording,
                            ));
                        if let Some(session) = self
                            .debug_dock
                            .as_ref()
                            .and_then(|d| d.session.clone())
                        {
                            std::thread::spawn(move || {
                                let _ = session.unload_script(active_id);
                            });
                        }
                        None
                    }
                    _ => None,
                }
            }
            glass_frida::SessionEvent::ScriptError { description, .. } => {
                self.coverage_recording = CoverageRecordingState::Failed(
                    description,
                );
                None
            }
            // ScriptLog / Detached can't reach here because
            // they didn't match `our_event` above.
            _ => Some(ev),
        }
    }
}
