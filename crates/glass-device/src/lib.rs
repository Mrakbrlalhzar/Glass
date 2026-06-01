//! Host-side discovery for USB-attached mobile devices.
//!
//! Two backends, one merged view:
//!
//!   * **ADB** — Android devices. Shells out to the `adb` CLI so
//!     Glass doesn't have to speak the binary protocol or manage
//!     the ADB daemon itself. Path discovery checks `$PATH`, the
//!     usual Android Studio + Homebrew install locations, and an
//!     optional user override.
//!   * **iDevice** — iOS devices. Uses the pure-Rust `idevice`
//!     crate which speaks the `usbmux` protocol directly. The
//!     crate is async (tokio); we wrap it behind a sync facade
//!     so callers don't have to drag a runtime into their code.
//!
//! The crate is intentionally GUI-free and free of any Glass
//! coupling beyond `anyhow` for errors. The GUI consumes it via
//! [`DeviceManager::list`] on a background poll tick.

use std::path::PathBuf;

pub mod adb;
pub mod idevice;
mod path;

/// A device's stable identifier. Combination of platform +
/// serial — adb serials and iOS UDIDs aren't guaranteed unique
/// across platforms, hence the prefix.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId {
    pub platform: DevicePlatform,
    pub serial: String,
}

impl DeviceId {
    pub fn android(serial: impl Into<String>) -> Self {
        Self {
            platform: DevicePlatform::Android,
            serial: serial.into(),
        }
    }
    pub fn ios(serial: impl Into<String>) -> Self {
        Self {
            platform: DevicePlatform::Ios,
            serial: serial.into(),
        }
    }
}

/// Which platform a device runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DevicePlatform {
    Android,
    Ios,
}

impl DevicePlatform {
    pub fn label(self) -> &'static str {
        match self {
            Self::Android => "Android",
            Self::Ios => "iOS",
        }
    }
}

/// How the device is connected. Today only USB; Wi-Fi
/// (`adb connect` / iOS over-Wi-Fi pairing) is a follow-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Usb,
}

/// Whether the device is ready for Glass to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthState {
    /// Ready to use — adb says `device`; iOS lockdown says
    /// pair-record present.
    Authorised,
    /// Plugged in, waiting on a user prompt. Android: "Allow
    /// USB debugging?"; iOS: "Trust this computer?".
    Unauthorised,
    /// Reported but not currently reachable — adb's `offline`
    /// state. Treated as a hint, not an error.
    Offline,
}

/// One device snapshot. Cloned cheaply; nothing borrows from
/// internal state.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: DeviceId,
    /// User-readable label: `Pixel 7`, `iPhone 15 Pro`. `None`
    /// when the device hasn't been authorised yet so we can't
    /// query model info.
    pub model: Option<String>,
    /// OS version string as the device reports it.
    pub os_version: Option<String>,
    pub transport: Transport,
    pub state: AuthState,
}

/// Top-level façade. Owns both backends; each is `Option` so a
/// missing tool on one side doesn't prevent the other from
/// listing. Errors during backend init are stored on
/// `BackendStatus` so the GUI can surface "ADB not found,
/// install platform tools" hints distinct from "no devices
/// plugged in".
pub struct DeviceManager {
    adb: Result<adb::AdbBackend, DeviceError>,
    ios: Result<idevice::IDeviceBackend, DeviceError>,
}

impl DeviceManager {
    /// Probe both backends. Cheap — backend constructors just
    /// look for binaries / open the usbmuxd socket once. Safe
    /// to call from any thread.
    pub fn new() -> Self {
        Self::with_adb_override(None)
    }

    /// Same as [`new`](Self::new) but lets the caller pin the
    /// adb binary location (typically from a settings.json field).
    /// Passing `None` falls back to the default discovery order.
    pub fn with_adb_override(adb_override: Option<PathBuf>) -> Self {
        let adb = adb::AdbBackend::with_override(adb_override);
        let ios = idevice::IDeviceBackend::new();
        Self { adb, ios }
    }

    /// Snapshot every reachable device, merged across both
    /// backends, in stable order (Android first, then iOS;
    /// within each, sorted by serial).
    pub fn list(&self) -> Vec<DeviceInfo> {
        let mut out = Vec::new();
        if let Ok(adb) = self.adb.as_ref() {
            match adb.list() {
                Ok(mut v) => {
                    v.sort_by(|a, b| a.id.serial.cmp(&b.id.serial));
                    out.extend(v);
                }
                Err(e) => tracing::warn!(?e, "glass-device: adb list failed"),
            }
        }
        if let Ok(ios) = self.ios.as_ref() {
            match ios.list() {
                Ok(mut v) => {
                    v.sort_by(|a, b| a.id.serial.cmp(&b.id.serial));
                    out.extend(v);
                }
                Err(e) => tracing::warn!(?e, "glass-device: ios list failed"),
            }
        }
        out
    }

    /// Find PIDs on an Android device whose process name matches
    /// `name`. Empty vec when nothing matches; `Err` only when
    /// adb itself failed. Used by the MCP `device-pidof` verb so
    /// an LLM can resolve a package name to the PID that
    /// `frida-attach` needs.
    pub fn android_pidof(
        &self,
        serial: &str,
        name: &str,
    ) -> Result<Vec<u32>, DeviceError> {
        match &self.adb {
            Ok(b) => b.pidof(serial, name),
            Err(e) => Err(e.clone()),
        }
    }

    /// Launch the main activity of `package` on an Android
    /// `serial`. Returns combined stdout+stderr from `monkey`.
    pub fn android_launch(
        &self,
        serial: &str,
        package: &str,
    ) -> Result<String, DeviceError> {
        match &self.adb {
            Ok(b) => b.start_main_activity(serial, package),
            Err(e) => Err(e.clone()),
        }
    }

    /// Force-stop every process belonging to `package`.
    pub fn android_force_stop(
        &self,
        serial: &str,
        package: &str,
    ) -> Result<String, DeviceError> {
        match &self.adb {
            Ok(b) => b.force_stop(serial, package),
            Err(e) => Err(e.clone()),
        }
    }

    /// Pull a file from an Android device to the host.
    pub fn android_pull(
        &self,
        serial: &str,
        remote: &str,
        local: &std::path::Path,
    ) -> Result<String, DeviceError> {
        match &self.adb {
            Ok(b) => b.pull(serial, remote, local),
            Err(e) => Err(e.clone()),
        }
    }

    /// Push a local file to an Android device.
    pub fn android_push(
        &self,
        serial: &str,
        local: &std::path::Path,
        remote: &str,
    ) -> Result<String, DeviceError> {
        match &self.adb {
            Ok(b) => b.push(serial, local, remote),
            Err(e) => Err(e.clone()),
        }
    }

    /// Run an arbitrary `adb shell` command and return stdout.
    pub fn android_shell(
        &self,
        serial: &str,
        args: &[&str],
    ) -> Result<String, DeviceError> {
        match &self.adb {
            Ok(b) => b.shell(serial, args),
            Err(e) => Err(e.clone()),
        }
    }

    pub fn backend_status(&self) -> BackendStatus {
        BackendStatus {
            adb: self
                .adb
                .as_ref()
                .map(|b| AdbInfo {
                    binary_path: b.binary_path().to_path_buf(),
                    version: b.version(),
                })
                .map_err(Clone::clone),
            ios: self
                .ios
                .as_ref()
                .map(|b| IosInfo {
                    usbmuxd_reachable: b.usbmuxd_reachable(),
                })
                .map_err(Clone::clone),
        }
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct BackendStatus {
    pub adb: Result<AdbInfo, DeviceError>,
    pub ios: Result<IosInfo, DeviceError>,
}

#[derive(Clone, Debug)]
pub struct AdbInfo {
    pub binary_path: PathBuf,
    /// First line of `adb version`. Informational, surfaced in
    /// settings; we don't parse a semver.
    pub version: String,
}

#[derive(Clone, Debug)]
pub struct IosInfo {
    pub usbmuxd_reachable: bool,
}

/// Cloneable, displayable error suitable for surfacing in the
/// device picker. We never expose internal `anyhow::Error`
/// shapes — every backend method returns this type.
#[derive(Clone, Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("adb binary not found — install Android Platform Tools or set `device.adb_path` in settings")]
    AdbNotFound,
    #[error("iOS backend unavailable: {0}")]
    IosBackendUnavailable(String),
    #[error("backend error: {0}")]
    Backend(String),
}
