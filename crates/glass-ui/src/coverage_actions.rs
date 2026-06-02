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
        // Three classes of event matter to an in-flight recording:
        //   * ScriptMessage with our script_id  → init / result
        //   * ScriptError with our script_id    → script blew up
        //   * Detached (no script_id)           → session died,
        //     usually because the target crashed under Stalker.
        //     Surface as Failed AND pass through so the dock log
        //     also shows the reason.
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
            if let glass_frida::SessionEvent::Detached { reason } = &ev {
                // Session-level detach. The Recording is dead
                // either way; flip state to Failed so the user
                // isn't stuck looking at "recording…" forever.
                // Return Some(ev) so the dock log still gets it.
                self.coverage_recording = CoverageRecordingState::Failed(
                    format!("session detached: {reason}"),
                );
            }
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
                        // No script-unload here. Spawning a
                        // detached `std::thread` to call
                        // `unload_script` from outside the
                        // gpui async runtime was racy — the
                        // script's `setTimeout` callback can
                        // still be unwinding inside frida-
                        // core's main loop when we yank the
                        // script out. The script has already
                        // done its work (Stalker.unfollow +
                        // flush + send), so leaving it loaded
                        // is at worst a small leak that the
                        // next `start_coverage_recording`
                        // cleans up explicitly.
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
            // ScriptLog never reaches here (didn't match
            // `our_event` above); the other variants are
            // exhaustive.
            _ => Some(ev),
        }
    }

    /// Watchdog: if we've been stuck in `Recording` for far
    /// longer than the requested duration plus a generous
    /// Stalker-teardown allowance, give up and surface Failed.
    /// Prevents the spinner-of-doom when the target crashes
    /// before delivering its final `send`. Called from the
    /// global event pump on every tick so it doesn't need a
    /// dedicated timer.
    pub(crate) fn coverage_watchdog_tick(&mut self) -> bool {
        let (started_at, duration_ms) = match &self.coverage_recording {
            CoverageRecordingState::Recording {
                started_at,
                duration_ms,
                ..
            } => (*started_at, *duration_ms),
            _ => return false,
        };
        // duration + 30s grace. Stalker teardown on a big
        // module set genuinely takes seconds; the watchdog is
        // for "the target died" not "Stalker is just slow".
        let deadline = std::time::Duration::from_millis(
            duration_ms.saturating_add(30_000),
        );
        if started_at.elapsed() >= deadline {
            self.coverage_recording = CoverageRecordingState::Failed(
                format!(
                    "no result within {}ms (target may have crashed under \
                     Stalker, or the session detached silently)",
                    duration_ms.saturating_add(30_000)
                ),
            );
            true
        } else {
            false
        }
    }
}
