//! Locate and drive the Android signing tools.
//!
//! Glass ships nothing for signing — we shell out to the
//! standard tools every Android developer already has:
//!
//!   * `keytool` — comes with any JDK install. Used to
//!     generate the Glass-specific debug keystore on first
//!     use.
//!   * `apksigner` — part of the Android SDK build-tools
//!     package. Signs APKs (v1/v2/v3) and re-zip-aligns.
//!
//! Both look-ups follow the same shape: PATH first, then
//! environment-variable conventions, then platform-specific
//! defaults. Missing tools surface as a typed [`SignError`]
//! so the dialog can render install instructions.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Glass's debug keystore lives under the platform data dir
/// (`~/Library/Application Support/Glass/` on macOS,
/// `$XDG_DATA_HOME/Glass/` on Linux). Reused across every
/// inject-and-install — same signature means future installs
/// can `-r` upgrade the previous one.
pub const KEYSTORE_FILENAME: &str = "glass-debug.keystore";
pub const KEYSTORE_ALIAS: &str = "glass";
/// Hard-coded password. Matches Android Studio's debug-keystore
/// convention (storepass=android, keypass=android). Lets users
/// inspect the keystore with the same `keytool` command they're
/// used to. This is a **debug** key — only ever used to sign
/// patched APKs for testing on your own devices.
pub const KEYSTORE_PASSWORD: &str = "android";

#[derive(Clone, Debug, thiserror::Error)]
pub enum SignError {
    #[error("`keytool` not found — install a JDK (e.g. `brew install openjdk` on macOS) or put keytool on PATH")]
    KeytoolNotFound,
    #[error("`apksigner` not found — install the Android SDK build-tools (e.g. via Android Studio's SDK Manager) and ensure $ANDROID_HOME is set")]
    ApksignerNotFound,
    #[error("couldn't determine the platform data directory for the keystore (no $HOME / $XDG_DATA_HOME)")]
    NoDataDir,
    #[error("keystore generation failed: {0}")]
    KeystoreGenFailed(String),
    #[error("apksigner failed: {0}")]
    ApksignerFailed(String),
}

/// Discovered locations of every tool we need. Build once at
/// the start of a sign-and-install run; reuse for each
/// invocation.
#[derive(Clone, Debug)]
pub struct SignerTools {
    pub keytool: PathBuf,
    pub apksigner: PathBuf,
    /// Where Glass will write (and re-read) the debug keystore.
    /// Created on first use by `ensure_keystore`.
    pub keystore_path: PathBuf,
}

impl SignerTools {
    pub fn discover() -> Result<Self, SignError> {
        let keytool = find_keytool().ok_or(SignError::KeytoolNotFound)?;
        let apksigner = find_apksigner().ok_or(SignError::ApksignerNotFound)?;
        let data_dir = data_dir().ok_or(SignError::NoDataDir)?;
        let keystore_path = data_dir.join("Glass").join(KEYSTORE_FILENAME);
        Ok(Self {
            keytool,
            apksigner,
            keystore_path,
        })
    }

    /// Generate the Glass debug keystore if it doesn't already
    /// exist. Idempotent — repeated calls after the first do
    /// nothing.
    pub fn ensure_keystore(&self) -> Result<(), SignError> {
        if self.keystore_path.is_file() {
            return Ok(());
        }
        if let Some(parent) = self.keystore_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SignError::KeystoreGenFailed(format!(
                    "creating {}: {e}",
                    parent.display()
                ))
            })?;
        }
        // `-genkeypair` with conservative defaults. Validity
        // 100 years so we never have to think about renewal
        // for a debug key. Distinguished name is fixed so the
        // keystore has the same subject every Glass install
        // generates.
        let output = Command::new(&self.keytool)
            .args([
                "-genkeypair",
                "-keystore",
                self.keystore_path.to_str().unwrap_or_default(),
                "-alias",
                KEYSTORE_ALIAS,
                "-keyalg",
                "RSA",
                "-keysize",
                "2048",
                "-validity",
                "36500",
                "-storepass",
                KEYSTORE_PASSWORD,
                "-keypass",
                KEYSTORE_PASSWORD,
                "-dname",
                "CN=Glass Debug, OU=Glass, O=Glass, L=Local, ST=Local, C=US",
            ])
            .output()
            .map_err(|e| {
                SignError::KeystoreGenFailed(format!("spawning keytool: {e}"))
            })?;
        if !output.status.success() {
            return Err(SignError::KeystoreGenFailed(format!(
                "keytool exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Sign `apk_path` in place using the Glass debug keystore.
    /// Caller must have run [`ensure_keystore`] at least once.
    /// Returns the combined stdout+stderr so the dialog can
    /// surface progress info; the APK on disk is the side
    /// effect.
    pub fn sign(&self, apk_path: &Path) -> Result<String, SignError> {
        let apk = apk_path.to_str().ok_or_else(|| {
            SignError::ApksignerFailed(format!(
                "non-UTF-8 APK path: {}",
                apk_path.display()
            ))
        })?;
        let output = Command::new(&self.apksigner)
            .args([
                "sign",
                "--ks",
                self.keystore_path.to_str().unwrap_or_default(),
                "--ks-key-alias",
                KEYSTORE_ALIAS,
                "--ks-pass",
                &format!("pass:{}", KEYSTORE_PASSWORD),
                "--key-pass",
                &format!("pass:{}", KEYSTORE_PASSWORD),
                apk,
            ])
            .output()
            .map_err(|e| {
                SignError::ApksignerFailed(format!("spawning apksigner: {e}"))
            })?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        if !output.status.success() {
            return Err(SignError::ApksignerFailed(format!(
                "apksigner exited {}: {}",
                output.status,
                combined.trim()
            )));
        }
        Ok(combined)
    }
}

fn find_keytool() -> Option<PathBuf> {
    if let Some(p) = scan_path(if cfg!(windows) { "keytool.exe" } else { "keytool" }) {
        return Some(p);
    }
    // `$JAVA_HOME/bin/keytool` is the standard JDK layout.
    if let Some(java_home) = std::env::var_os("JAVA_HOME") {
        let candidate = PathBuf::from(java_home).join("bin").join(
            if cfg!(windows) { "keytool.exe" } else { "keytool" },
        );
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    #[cfg(target_os = "macos")]
    {
        // macOS hides the system JDK behind /usr/libexec/java_home.
        // Don't shell out for it here — too slow and brittle.
        // Users without JAVA_HOME / PATH set will see the
        // typed KeytoolNotFound error and the install hint.
    }
    None
}

fn find_apksigner() -> Option<PathBuf> {
    let name =
        if cfg!(windows) { "apksigner.bat" } else { "apksigner" };
    if let Some(p) = scan_path(name) {
        return Some(p);
    }
    // Standard SDK layout: $ANDROID_HOME/build-tools/<version>/apksigner.
    // We don't know which build-tools version is installed —
    // pick the lexicographically-largest dir, which is also the
    // version-largest because Android build-tools versions sort
    // sensibly (34.0.0, 35.0.0, …).
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = std::env::var_os(var) {
            let bt = PathBuf::from(root).join("build-tools");
            if let Some(latest) = latest_subdir(&bt) {
                let candidate = latest.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    // macOS / Linux Android Studio defaults.
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        let defaults = [
            home.join("Library/Android/sdk/build-tools"),
            home.join("Android/Sdk/build-tools"),
        ];
        for bt in defaults {
            if let Some(latest) = latest_subdir(&bt) {
                let candidate = latest.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn latest_subdir(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut versions: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    versions.sort();
    versions.into_iter().last()
}

fn scan_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path_var) {
        let candidate = entry.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join("Library/Application Support"));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return Some(PathBuf::from(xdg));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join(".local").join("share"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_resolves_on_macos() {
        #[cfg(target_os = "macos")]
        {
            let original = std::env::var_os("HOME");
            // SAFETY: tests in this crate are single-threaded.
            unsafe {
                std::env::set_var("HOME", "/tmp/glass-test-home");
            }
            let dir = data_dir().expect("data dir resolves");
            assert!(dir.to_string_lossy().contains("Application Support"));
            unsafe {
                match original {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    #[test]
    fn latest_subdir_picks_highest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for v in ["30.0.3", "34.0.0", "35.0.0", "33.0.2"] {
            std::fs::create_dir(tmp.path().join(v)).unwrap();
        }
        let latest = latest_subdir(tmp.path()).expect("found");
        assert!(latest.to_string_lossy().ends_with("35.0.0"));
    }
}
