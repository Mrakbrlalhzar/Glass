//! Filesystem hunt for the `adb` binary.
//!
//! Discovery order — first hit wins:
//!
//!   1. Explicit override path (from settings.json or
//!      `DeviceManager::with_adb_override`). Returned even if
//!      it doesn't exist so misconfiguration surfaces clearly
//!      rather than silently falling back to `$PATH`.
//!   2. `$PATH` — covers Homebrew (`brew install
//!      android-platform-tools`) and any other manual install
//!      that put `adb` on the user's shell PATH.
//!   3. `$ANDROID_HOME/platform-tools/adb`.
//!   4. `$ANDROID_SDK_ROOT/platform-tools/adb`.
//!   5. Platform-typical Android Studio install locations.
//!   6. Homebrew prefixes for completeness (already covered by
//!      `$PATH` on most setups, but here as a backstop).
//!
//! No system call beyond `metadata` per candidate. Cheap.

use std::path::{Path, PathBuf};

const ADB_EXE: &str = if cfg!(windows) { "adb.exe" } else { "adb" };

pub fn discover_adb(override_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = override_path {
        return Some(p.to_path_buf());
    }
    if let Some(p) = scan_path() {
        return Some(p);
    }
    for candidate in sdk_candidates() {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn scan_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path_var) {
        let candidate = entry.join(ADB_EXE);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn sdk_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(val) = std::env::var_os(var) {
            out.push(PathBuf::from(val).join("platform-tools").join(ADB_EXE));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        // Android Studio's macOS default install location.
        out.push(
            home.join("Library/Android/sdk/platform-tools").join(ADB_EXE),
        );
        // Android Studio's Linux default install location.
        out.push(home.join("Android/Sdk/platform-tools").join(ADB_EXE));
    }
    #[cfg(target_os = "macos")]
    {
        out.push(PathBuf::from("/opt/homebrew/bin").join(ADB_EXE));
        out.push(PathBuf::from("/usr/local/bin").join(ADB_EXE));
    }
    #[cfg(windows)]
    {
        if let Some(local_app) = std::env::var_os("LOCALAPPDATA") {
            out.push(
                PathBuf::from(local_app)
                    .join("Android/Sdk/platform-tools")
                    .join(ADB_EXE),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_returns_as_is_even_when_missing() {
        let p = PathBuf::from("/nonsense/adb");
        let found = discover_adb(Some(&p));
        assert_eq!(found, Some(p));
    }

    #[test]
    fn sdk_candidates_include_macos_default_when_home_set() {
        // Just probe the helper directly — we don't want this
        // test to depend on a real adb being present.
        // Set HOME for the test scope.
        let original = std::env::var_os("HOME");
        // SAFETY: tests in this crate are single-threaded so
        // env-var mutation is OK. Restored at end.
        unsafe {
            std::env::set_var("HOME", "/tmp/fake");
        }
        let candidates = sdk_candidates();
        let any_macos = candidates.iter().any(|p| {
            p.to_string_lossy()
                .contains("Library/Android/sdk/platform-tools")
        });
        assert!(any_macos, "expected macOS Android Studio path in {candidates:?}");
        unsafe {
            match original {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
