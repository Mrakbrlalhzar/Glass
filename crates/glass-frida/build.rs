//! Read the Frida release version from `frida-sys`'s pinned
//! `FRIDA_VERSION` file and re-export it as the
//! `GLASS_FRIDA_VERSION` env var so the runtime can fetch a
//! matching `frida-server` for the device.
//!
//! Why probe `frida-sys` instead of hardcoding? When we bump the
//! `frida` Cargo dep the host devkit version changes
//! automatically; without this build script the device-side
//! server would silently drift out of sync with the linked
//! libraries until someone noticed at runtime.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    // The version is only meaningful when we actually link
    // against frida-sys (the `frida` feature). For featureless
    // builds we emit "unknown" so the runtime can report a
    // clear error rather than offering to download a phantom
    // release.
    let version = if env::var("CARGO_FEATURE_FRIDA").is_ok() {
        read_frida_sys_version().unwrap_or_else(|e| {
            println!("cargo:warning=glass-frida: could not read frida-sys FRIDA_VERSION: {e}");
            "unknown".to_string()
        })
    } else {
        "unknown".to_string()
    };
    println!("cargo:rustc-env=GLASS_FRIDA_VERSION={version}");
    println!("cargo:rerun-if-changed=build.rs");
}

/// Find the active `frida-sys` source dir under
/// `~/.cargo/registry/src` and read its `FRIDA_VERSION` file.
/// The lockfile points at a specific version (e.g.
/// `frida-sys-0.17.2`); we look for any `frida-sys-*` directory
/// under any cargo registry checkout and pick the lexically
/// highest one — there's typically only one but multi-version
/// caches can have more.
fn read_frida_sys_version() -> Result<String, String> {
    let home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .ok_or_else(|| "no CARGO_HOME or HOME".to_string())?;
    let registry_src = home.join("registry").join("src");
    let mut best: Option<PathBuf> = None;
    let entries = fs::read_dir(&registry_src)
        .map_err(|e| format!("reading {}: {e}", registry_src.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Each registry index gets its own subdir; scan inside
        // for `frida-sys-*` source checkouts.
        let inner = match fs::read_dir(&path) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for child in inner.flatten() {
            let name = child.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("frida-sys-") {
                let candidate = child.path();
                if best.as_ref().is_none_or(|b| candidate > *b) {
                    best = Some(candidate);
                }
            }
        }
    }
    let dir = best.ok_or_else(|| {
        "no frida-sys-* checkout under ~/.cargo/registry/src".to_string()
    })?;
    let version_path = dir.join("FRIDA_VERSION");
    let raw = fs::read_to_string(&version_path)
        .map_err(|e| format!("reading {}: {e}", version_path.display()))?;
    Ok(raw.trim().to_string())
}
