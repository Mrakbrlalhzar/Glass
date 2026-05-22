//! ADB backend — shells out to the `adb` CLI.
//!
//! We don't speak the ADB wire protocol directly. Reasons:
//!   * `adb` is already installed by anyone doing Android
//!     reverse engineering; one fewer thing for Glass to vendor.
//!   * The wire protocol still requires a running `adb-server`
//!     daemon (the binary protocol crates talk *to* the daemon,
//!     not the device), so going binary doesn't actually remove
//!     any external dependency.
//!   * Easier debugging — every command we run, the user can
//!     reproduce in their terminal.
//!
//! The backend caches per-serial OS-version lookups so we don't
//! shell `adb shell getprop` on every refresh tick.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use crate::{path, AuthState, DeviceError, DeviceId, DeviceInfo, Transport};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct AdbBackend {
    binary: PathBuf,
    version: String,
    /// `serial -> (model, os_version)` cache. Populated lazily
    /// on first sighting; reused on subsequent polls. We don't
    /// invalidate — the same physical device giving us a
    /// different model would be too unusual to plan for.
    cache: std::sync::Arc<Mutex<HashMap<String, CachedProps>>>,
}

#[derive(Clone, Default)]
struct CachedProps {
    model: Option<String>,
    os_version: Option<String>,
}

impl AdbBackend {
    pub fn with_override(
        override_path: Option<PathBuf>,
    ) -> Result<Self, DeviceError> {
        let binary = path::discover_adb(override_path.as_deref())
            .ok_or(DeviceError::AdbNotFound)?;
        // Sanity-probe with `adb version`. If the binary exists
        // but the user's machine can't run it (Rosetta missing,
        // wrong arch, permissions), bail with a Backend error
        // rather than the misleading "not found".
        let version = run_capture(&binary, &["version"]).map_err(|e| {
            DeviceError::Backend(format!("`adb version` failed: {e}"))
        })?;
        // Trim to the first line for tidiness.
        let version = version
            .lines()
            .next()
            .unwrap_or("adb (unknown version)")
            .to_string();
        Ok(Self {
            binary,
            version,
            cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn binary_path(&self) -> &Path {
        &self.binary
    }

    pub fn version(&self) -> String {
        self.version.clone()
    }

    /// Snapshot every device currently reachable to the local
    /// adb daemon. Order matches `adb devices -l`.
    pub fn list(&self) -> Result<Vec<DeviceInfo>, DeviceError> {
        let raw = run_capture(&self.binary, &["devices", "-l"])
            .map_err(|e| DeviceError::Backend(e.to_string()))?;
        let parsed = parse_devices(&raw);
        let mut out = Vec::with_capacity(parsed.len());
        for line in parsed {
            let mut info = DeviceInfo {
                id: DeviceId::android(&line.serial),
                model: line.model,
                os_version: None,
                transport: Transport::Usb,
                state: line.state,
            };
            // Only query OS version for authorised devices —
            // unauthorised / offline devices reject the shell
            // command anyway, and there's nothing to display.
            if matches!(info.state, AuthState::Authorised) {
                let props = self.props_for(&line.serial);
                if info.model.is_none() {
                    info.model = props.model.clone();
                }
                info.os_version = props.os_version.clone();
            }
            out.push(info);
        }
        Ok(out)
    }

    fn props_for(&self, serial: &str) -> CachedProps {
        if let Some(cached) = self
            .cache
            .lock()
            .ok()
            .and_then(|c| c.get(serial).cloned())
        {
            return cached;
        }
        let mut props = CachedProps::default();
        if let Ok(out) = run_capture(
            &self.binary,
            &["-s", serial, "shell", "getprop", "ro.product.model"],
        ) {
            let trimmed = out.trim();
            if !trimmed.is_empty() {
                props.model = Some(trimmed.to_string());
            }
        }
        if let Ok(out) = run_capture(
            &self.binary,
            &[
                "-s",
                serial,
                "shell",
                "getprop",
                "ro.build.version.release",
            ],
        ) {
            let trimmed = out.trim();
            if !trimmed.is_empty() {
                props.os_version = Some(trimmed.to_string());
            }
        }
        if let Ok(mut c) = self.cache.lock() {
            c.insert(serial.to_string(), props.clone());
        }
        props
    }

    /// Run an arbitrary `adb shell` command and return its
    /// stdout. Used by callers that need quick visibility into
    /// device-side state without going through Frida — e.g.
    /// "is this package's process actually running" or "what's
    /// the gadget's listening port."
    pub fn shell(
        &self,
        serial: &str,
        args: &[&str],
    ) -> Result<String, crate::DeviceError> {
        let mut full = vec!["-s", serial, "shell"];
        full.extend_from_slice(args);
        run_capture(&self.binary, &full).map_err(crate::DeviceError::Backend)
    }

    /// Whether a process whose `cmdline` matches `name` is
    /// currently running on the device. Uses `pidof`, falling
    /// back to grep against `ps -A` for devices where `pidof`
    /// isn't on PATH. Returns Ok(false) on missing-process,
    /// Err only on tool-level failures so callers can default
    /// to "not running" safely.
    pub fn is_process_running(
        &self,
        serial: &str,
        name: &str,
    ) -> Result<bool, crate::DeviceError> {
        // Three-pronged check, because Android process matching
        // is unexpectedly fiddly:
        //
        //   1. `pidof <name>` — fast, works for daemons like
        //      `frida-server` whose `comm` is the binary name.
        //   2. `ps -A -o NAME` substring match — catches
        //      package-name-comm processes even when pidof
        //      missed them due to comm-truncation (Linux
        //      comm is capped at 15 bytes).
        //   3. `ps -A -o NAME,CMDLINE` cmdline match — catches
        //      cases where the comm differs from the package
        //      (e.g. frida-gadget hosted apps where the comm
        //      can be the gadget's config name).
        let output = std::process::Command::new(&self.binary)
            .args(["-s", serial, "shell", "pidof", name])
            .output()
            .map_err(|e| {
                crate::DeviceError::Backend(format!("spawning adb: {e}"))
            })?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().is_empty() {
                return Ok(true);
            }
        }
        // `ps -A` gives every process; the NAME column carries
        // the truncated comm. Substring match here so a
        // 15-byte-truncated `com.example.lon` still matches a
        // search for `com.example.longname`.
        let ps = std::process::Command::new(&self.binary)
            .args(["-s", serial, "shell", "ps", "-A", "-o", "NAME"])
            .output()
            .map_err(|e| {
                crate::DeviceError::Backend(format!("spawning adb: {e}"))
            })?;
        if ps.status.success() {
            let stdout = String::from_utf8_lossy(&ps.stdout);
            let needle = comm_truncate(name);
            if stdout.lines().any(|l| {
                let l = l.trim();
                l == name || l == needle || (l.len() >= 8 && name.starts_with(l))
            }) {
                return Ok(true);
            }
        }
        // Final fallback: look at the full cmdline. Slow on
        // big device process tables but unambiguous —
        // package processes always have their full name in
        // the cmdline regardless of comm truncation.
        let cmd = std::process::Command::new(&self.binary)
            .args(["-s", serial, "shell", "ps", "-A", "-o", "CMDLINE"])
            .output()
            .map_err(|e| {
                crate::DeviceError::Backend(format!("spawning adb: {e}"))
            })?;
        if cmd.status.success() {
            let stdout = String::from_utf8_lossy(&cmd.stdout);
            return Ok(stdout.lines().any(|l| {
                let l = l.trim();
                l == name || l.starts_with(&format!("{name} "))
            }));
        }
        Ok(false)
    }

    /// Launch the main activity of `package` on `serial`.
    /// Uses `monkey -p <pkg> -c LAUNCHER 1` so the caller
    /// doesn't have to know the activity name — `monkey`
    /// asks the package manager for the launcher activity
    /// and dispatches an intent matching the standard
    /// `MAIN`+`LAUNCHER` filter. Returns combined
    /// stdout+stderr for the debug-dock log pane.
    pub fn start_main_activity(
        &self,
        serial: &str,
        package: &str,
    ) -> Result<String, crate::DeviceError> {
        run_capture_combined(
            &self.binary,
            &[
                "-s",
                serial,
                "shell",
                "monkey",
                "-p",
                package,
                "-c",
                "android.intent.category.LAUNCHER",
                "1",
            ],
        )
        .map_err(crate::DeviceError::Backend)
    }

    /// Force-stop every process belonging to `package`. The
    /// device's package manager kills all of the app's
    /// processes immediately, even ones that have been
    /// backgrounded for hours.
    pub fn force_stop(
        &self,
        serial: &str,
        package: &str,
    ) -> Result<String, crate::DeviceError> {
        run_capture_combined(
            &self.binary,
            &["-s", serial, "shell", "am", "force-stop", package],
        )
        .map_err(crate::DeviceError::Backend)
    }

    /// Probe whether a Frida-gadget is currently listening on
    /// the device. Sets up an ADB TCP forward to port 27042
    /// (the gadget's default) and probes the device-side
    /// socket state through it.
    ///
    /// Why this is more involved than "TCP connect succeeded":
    /// ADB's host-side forward listener accepts the connect
    /// immediately regardless of whether the device-side port
    /// is actually bound — it lazily tries to connect to the
    /// device port and silently drops the link on failure. So
    /// a successful `connect()` tells us nothing about the
    /// gadget. We have to send a byte through the forward and
    /// observe what happens:
    ///
    ///   * If the device-side port is closed: ADB tunnels the
    ///     write, the device-side connect fails, ADB closes
    ///     the host-side connection cleanly. Our follow-up
    ///     `recv()` returns 0 bytes (EOF).
    ///   * If the device-side port is open (gadget listening
    ///     on 27042): ADB tunnels the write, the gadget
    ///     accepts the byte. Frida's protocol is client-
    ///     initiated and won't reply unprompted, so our
    ///     `recv()` times out — but the connection stays open
    ///     instead of giving us EOF.
    ///
    /// So: connect, send a byte, try to read with a short
    /// timeout. EOF → no gadget; timeout → gadget alive;
    /// data → gadget alive.
    pub fn probe_gadget(
        &self,
        serial: &str,
    ) -> Result<bool, crate::DeviceError> {
        const HOST_PORT: u16 = 27442;
        let host_spec = format!("tcp:{HOST_PORT}");
        let device_spec = "tcp:27042";
        let output = std::process::Command::new(&self.binary)
            .args(["-s", serial, "forward", &host_spec, device_spec])
            .output()
            .map_err(|e| {
                crate::DeviceError::Backend(format!("spawning adb forward: {e}"))
            })?;
        if !output.status.success() {
            return Err(crate::DeviceError::Backend(format!(
                "adb forward failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        use std::io::{Read, Write};
        use std::net::{SocketAddr, TcpStream};
        use std::time::Duration;
        let addr: SocketAddr = match format!("127.0.0.1:{HOST_PORT}").parse() {
            Ok(a) => a,
            Err(_) => return Ok(false),
        };
        let mut stream =
            match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
                Ok(s) => s,
                Err(_) => return Ok(false),
            };
        stream
            .set_read_timeout(Some(Duration::from_millis(600)))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_millis(500)))
            .ok();
        // Write a single byte through the ADB tunnel. If the
        // device-side dest port is closed, this triggers the
        // lazy device-side connect to fail, which causes ADB
        // to drop the host side. If the gadget is listening,
        // it accepts the byte silently.
        if stream.write_all(&[0u8]).is_err() {
            return Ok(false);
        }
        let mut buf = [0u8; 1];
        match stream.read(&mut buf) {
            // 0 bytes = clean EOF. Means ADB closed the
            // connection because the device-side connect
            // failed — no gadget.
            Ok(0) => Ok(false),
            // Any data = a real reply, definitely a gadget.
            Ok(_) => Ok(true),
            // Timeout = the gadget accepted our byte and is
            // waiting for more. That's the expected steady
            // state for Frida's client-initiated protocol;
            // it counts as "alive."
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(true)
            }
            // Other I/O error (connection reset, etc.) — the
            // safe default is "not running."
            Err(_) => Ok(false),
        }
    }

    /// Install (or reinstall) an APK on `serial`. `-r` keeps
    /// app data when the signatures match; if they don't, adb
    /// exits with `INSTALL_FAILED_UPDATE_INCOMPATIBLE` and the
    /// user has to uninstall the original first. The combined
    /// stdout+stderr is returned so the dialog can render the
    /// raw output (including the helpful Failure code on
    /// errors).
    pub fn install(
        &self,
        serial: &str,
        apk_path: &Path,
    ) -> Result<String, crate::DeviceError> {
        let apk_str = apk_path.to_str().ok_or_else(|| {
            crate::DeviceError::Backend(format!(
                "non-UTF-8 APK path: {}",
                apk_path.display(),
            ))
        })?;
        run_capture_combined(
            &self.binary,
            &["-s", serial, "install", "-r", apk_str],
        )
        .map_err(crate::DeviceError::Backend)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedLine {
    serial: String,
    state: AuthState,
    /// Populated from the `-l` extra fields (`model:...`) when
    /// the device is listed authorised — adb withholds those
    /// from `unauthorized` rows.
    model: Option<String>,
}

/// Parse `adb devices -l` output. Robust against the
/// "daemon not running. starting it now…" preamble adb prints
/// on first run, against the trailing blank line, and against
/// the `transport_id:` field newer adb versions add.
fn parse_devices(raw: &str) -> Vec<ParsedLine> {
    let mut out = Vec::new();
    let mut saw_header = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !saw_header {
            if line.starts_with("List of devices") {
                saw_header = true;
            }
            continue;
        }
        // After the header, each device is one line:
        //   ABC123  device   product:cheetah model:Pixel_7 …
        //   DEF456  unauthorized
        //   GHI789  offline
        let mut iter = line.split_whitespace();
        let serial = match iter.next() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let state_token = match iter.next() {
            Some(s) => s,
            None => continue,
        };
        let state = match state_token {
            "device" => AuthState::Authorised,
            "unauthorized" => AuthState::Unauthorised,
            "offline" => AuthState::Offline,
            _ => AuthState::Offline, // covers "no permissions" etc.
        };
        let mut model: Option<String> = None;
        for kv in iter {
            if let Some(value) = kv.strip_prefix("model:") {
                // adb reports underscores in device names; users
                // expect spaces.
                model = Some(value.replace('_', " "));
            }
        }
        out.push(ParsedLine { serial, state, model });
    }
    out
}

/// Variant of `run_capture` that returns stdout+stderr together
/// so installation logs come through even when adb succeeds.
/// `adb install` prints its summary ("Success") on stdout, and
/// failure detail ("Failure [INSTALL_FAILED_…]") on stderr — we
/// want both visible in the dialog status pane.
fn run_capture_combined(
    binary: &Path,
    args: &[&str],
) -> Result<String, String> {
    let output = Command::new(binary)
        .args(args)
        .output()
        .map_err(|e| format!("spawning adb: {e}"))?;
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    if !output.status.success() {
        return Err(format!(
            "adb exited {}: {}",
            output.status,
            combined.trim()
        ));
    }
    // `adb install` exits 0 on a "Failure [...]" too, so look
    // at the stdout content as well. If the user sees
    // "Success" we treat it as a success.
    if !combined.contains("Success") && combined.contains("Failure") {
        return Err(combined.trim().to_string());
    }
    Ok(combined)
}

/// Truncate a name to what Linux stores in `/proc/<pid>/comm`
/// (and what `ps -o NAME` shows): 15 bytes plus a null. Used
/// when looking up Android package processes by comm.
fn comm_truncate(name: &str) -> String {
    if name.len() <= 15 {
        name.to_string()
    } else {
        name[..15].to_string()
    }
}

fn run_capture(binary: &Path, args: &[&str]) -> Result<String, String> {
    // We could enforce a timeout via threads + child kill, but
    // adb subcommands we run are O(local socket) — they return
    // within milliseconds on healthy systems. A timeout would
    // mostly mask real issues. If a `getprop` hangs because the
    // device froze, the GUI's poll-task will simply skip that
    // tick.
    let _ = COMMAND_TIMEOUT; // reserved for future use
    let output = Command::new(binary)
        .args(args)
        .output()
        .map_err(|e| format!("spawning adb: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("adb exited {}: {}", output.status, stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_output() {
        let raw = "List of devices attached\n\
            ABC1234567       device product:cheetah model:Pixel_7 device:cheetah transport_id:1\n\
            DEF5678901       unauthorized\n\
            ZZ9876543210     offline\n";
        let parsed = parse_devices(raw);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].serial, "ABC1234567");
        assert_eq!(parsed[0].state, AuthState::Authorised);
        assert_eq!(parsed[0].model.as_deref(), Some("Pixel 7"));
        assert_eq!(parsed[1].serial, "DEF5678901");
        assert_eq!(parsed[1].state, AuthState::Unauthorised);
        assert_eq!(parsed[1].model, None);
        assert_eq!(parsed[2].state, AuthState::Offline);
    }

    #[test]
    fn ignores_daemon_preamble() {
        let raw = "* daemon not running. starting it now on port 5037 *\n\
            * daemon started successfully *\n\
            List of devices attached\n\
            ABC1234567       device\n";
        let parsed = parse_devices(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].serial, "ABC1234567");
    }

    #[test]
    fn empty_when_no_devices() {
        let raw = "List of devices attached\n\n";
        assert!(parse_devices(raw).is_empty());
    }

    #[test]
    fn handles_no_permissions_lines() {
        // adb sometimes emits this on Linux when udev rules
        // aren't installed. We treat it as offline so the
        // GUI surfaces "device unreachable" rather than
        // crashing on an unexpected state token.
        let raw = "List of devices attached\n\
            ABC1234567       no\n";
        let parsed = parse_devices(raw);
        assert_eq!(parsed[0].state, AuthState::Offline);
    }
}
