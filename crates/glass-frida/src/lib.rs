//! Host-side Frida driver.
//!
//! Thin wrapper around the `frida` crate. Linked unconditionally
//! — Frida is an essential capability for Glass, not opt-in.
//! `frida-sys`'s build script downloads the matching
//! `frida-core-devkit` tarball from frida's GitHub releases on
//! first build (~30MB, cached in `target/` after).

use std::sync::OnceLock;

use glass_device::DeviceId;

pub mod gadgets;
pub mod injection;
pub mod patch;
pub mod server;
pub mod session;
pub mod sign;
pub mod trace_js;
pub use server::{
    asset_name as frida_server_asset_name, asset_url as frida_server_asset_url,
    stage_server, AndroidServerArch, ServerStageError, StageProgress,
    FRIDA_VERSION,
};
pub use gadgets::{
    android_gadget_config_listen, for_android_abi, GadgetBinary,
    ANDROID_GADGET_CONFIG_FILENAME,
};
pub use injection::{
    plan_injection, InjectionPlan, PatchMethod, PatchTarget, PlanError, PlanInputs,
    PlanWarning,
};
pub use patch::{apply_plan, PatchError, GADGET_LIBRARY_NAME};
pub use session::{AttachReport, ScriptId, Session, SessionEvent, SpawnReport};
pub use trace_js::{
    build_bridged_script, render_hook_script, render_trace_script, HookBody, JsRenderError,
};
pub use sign::{SignError, SignerTools};

/// Cloneable error type surfaced through the GUI.
#[derive(Clone, Debug, thiserror::Error)]
pub enum FridaError {
    #[error("Frida runtime initialisation failed: {0}")]
    InitFailed(String),
    #[error("device not found in Frida's device list (serial {0})")]
    DeviceNotFound(String),
    #[error("frida-server not reachable on the device — install frida-server (rooted) or inject the gadget into the APK")]
    ServerUnreachable,
    #[error("frida error: {0}")]
    Backend(String),
}

/// Result of [`FridaRuntime::probe`]. Surfaced as the chip's
/// secondary line so the user knows whether tracing will work
/// before they try.
#[derive(Clone, Debug)]
pub struct ProbeReport {
    /// What flavour of Frida we found on the device. Distinguishes
    /// "frida-server (rooted, system-wide instrumentation)" from
    /// "no full Frida, but the gadget might be loaded in a
    /// specific process" from "no Frida at all".
    pub kind: FridaKind,
    /// Frida agent version reported by `query_system_parameters`,
    /// when available. Informational; the source is the
    /// device-side runtime, not the gum / core libraries on the
    /// host.
    pub agent_version: Option<String>,
    /// OS reported by frida-server. Useful for surfacing
    /// "android 14 / ios 17.4" alongside the version chip.
    pub os: Option<String>,
    /// In gadget mode: the process names Frida reported as
    /// hosting gadgets. Used by the chip to cross-check
    /// against `adb shell pidof <package>` — frida-core caches
    /// process listings aggressively and continues to enumerate
    /// gadgeted apps after they've been killed or uninstalled,
    /// so the host needs an independent signal to confirm one
    /// of these processes is *actually* alive.
    pub gadget_process_names: Vec<String>,
}

/// What we actually found on the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FridaKind {
    /// `frida-server` process detected in the device's process
    /// list. Glass can attach to any process system-wide
    /// (typically requires root).
    Server,
    /// No `frida-server` process found, but `query_system_
    /// parameters` succeeded — almost always means the gadget
    /// is loaded into one of the running apps and is reachable
    /// over USB. Glass can attach to that app's process only.
    Gadget,
}

/// Process-wide Frida handle. `frida::Frida::obtain()` is
/// `unsafe` and must be called once for the entire process; we
/// stash the result in a `OnceLock` so subsequent callers get
/// the same instance.
pub struct FridaRuntime {
    _inner: &'static frida::Frida,
}

impl FridaRuntime {
    /// Acquire (or lazily initialise) the global Frida runtime.
    /// Safe to call from any thread; the underlying
    /// `Frida::obtain` is only invoked once.
    pub fn get() -> Result<&'static FridaRuntime, FridaError> {
        static FRIDA: OnceLock<frida::Frida> = OnceLock::new();
        static RUNTIME: OnceLock<FridaRuntime> = OnceLock::new();
        let rt = RUNTIME.get_or_init(|| {
            // SAFETY: `Frida::obtain` is sound the first
            // time it's called from a single thread of
            // execution. The `OnceLock` guarantees the
            // initialiser body runs exactly once; subsequent
            // callers see the already-initialised instance.
            let inner = FRIDA.get_or_init(|| unsafe { frida::Frida::obtain() });
            FridaRuntime { _inner: inner }
        });
        Ok(rt)
    }

    /// Probe a device for a running frida-server.
    ///
    /// Returns:
    ///   * `Ok(ProbeReport { … })` — frida-server is reachable.
    ///     The user can immediately attach and start tracing.
    ///   * `Err(FridaError::DeviceNotFound)` — Frida's device
    ///     manager doesn't see this serial. Cable problem,
    ///     debugging disabled, etc.
    ///   * `Err(FridaError::ServerUnreachable)` — Frida sees
    ///     the device but the connection to frida-server times
    ///     out or refuses. User needs to either start
    ///     frida-server (rooted devices) or inject the gadget
    ///     (stock devices). The chip surfaces a hint.
    ///
    /// Blocking; run on a background thread.
    pub fn probe(device: &DeviceId) -> Result<ProbeReport, FridaError> {
        // Wrap the whole inner call in catch_unwind too —
        // frida-core can panic deep inside enumerate /
        // attach paths when a device-side connection has
        // died but its host-side state hasn't cleaned up
        // yet. Treat any panic as "unreachable" rather
        // than crashing the GUI thread.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Self::probe_inner(device)
        }))
        .unwrap_or(Err(FridaError::ServerUnreachable))
    }

    fn probe_inner(device: &DeviceId) -> Result<ProbeReport, FridaError> {
        let rt = Self::get()?;
        let mgr = frida::DeviceManager::obtain(rt._inner);
        let devices = mgr.enumerate_all_devices();
        // Skip Frida's `local` (the host) device — we only
        // want the USB-attached phone whose serial / UDID
        // matches glass-device's pick. Without this filter a
        // mismatched USB serial would silently fall through
        // to host-side Frida and show as "reachable".
        let target = devices
            .iter()
            .find(|d| {
                d.get_id() == device.serial
                    && matches!(d.get_type(), frida::DeviceType::USB)
            })
            .ok_or_else(|| FridaError::DeviceNotFound(device.serial.clone()))?;
        // Frida-core's `enumerate_processes` returns the
        // device's whole `ps` table on any reachable Android,
        // so a non-empty list isn't a useful "Frida is alive"
        // signal — we can only trust it to surface a
        // `frida-server` daemon.
        //
        // Decision:
        //   * `frida-server` in process list → Server.
        //   * Otherwise → unreachable here. The caller
        //     (glass-ui) does an independent gadget-port
        //     probe (`adb forward` to 27042) and upgrades to
        //     Gadget if the port answers.
        let processes = std::panic::catch_unwind(
            std::panic::AssertUnwindSafe(|| target.enumerate_processes()),
        )
        .map_err(|_| FridaError::ServerUnreachable)?;
        let server_entry = processes
            .iter()
            .find(|p| p.get_name() == "frida-server");
        let kind = if server_entry.is_some() {
            FridaKind::Server
        } else {
            return Err(FridaError::ServerUnreachable);
        };
        // `query_system_parameters` is reliable in either mode
        // — server returns its own version, gadget returns the
        // injected app's. Don't fail the whole probe if it
        // errors though; the kind we already established is
        // the chip's most important signal.
        let params = target.query_system_parameters().ok();
        let agent_version = params
            .as_ref()
            .and_then(|p| p.get("version"))
            .and_then(|v| v.get_string())
            .map(|s| s.to_string());
        let os = params
            .as_ref()
            .and_then(|p| p.get("os"))
            .and_then(|v| match v {
                frida::Variant::Map(m) => m.get("id").and_then(|x| x.get_string()),
                _ => None,
            })
            .map(|s| s.to_string());
        Ok(ProbeReport {
            kind,
            agent_version,
            os,
            gadget_process_names: Vec::new(),
        })
    }
}
