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
    // frida-sys is now unconditional, so the FRIDA_VERSION lookup
    // is too. Emit "unknown" only as a defensive fallback when
    // the lookup fails — that surfaces as a clear runtime error
    // rather than a phantom-release download.
    let version = read_frida_sys_version().unwrap_or_else(|e| {
        println!("cargo:warning=glass-frida: could not read frida-sys FRIDA_VERSION: {e}");
        "unknown".to_string()
    });
    println!("cargo:rustc-env=GLASS_FRIDA_VERSION={version}");
    println!("cargo:rerun-if-changed=build.rs");

    link_glib();
}

/// Emit the system GLib link flags on Linux.
///
/// Our hand-rolled FFI in `src/glib_ffi.rs` references GLib
/// symbols (`g_object_unref`, `g_signal_connect_data`, the
/// `g_main_context_*` family, …). On macOS the Frida devkit
/// bundles GLib statically, so those symbols resolve from
/// `libfrida-core` and no directive is needed. On Linux the
/// devkit links GLib dynamically and leaves the symbols to the
/// system libraries, so without these directives the final
/// binary fails at link time with `undefined symbol: g_*`.
///
/// `pkg-config` is used (rather than a bare `rustc-link-lib`) so
/// non-standard library locations are picked up via the proper
/// `-L` search paths.
fn link_glib() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" {
        return;
    }
    // `gobject-2.0` pulls in `glib-2.0` transitively, but probe
    // both explicitly so a missing package names itself clearly.
    for lib in ["glib-2.0", "gobject-2.0"] {
        if let Err(e) = pkg_config::Config::new().probe(lib) {
            panic!(
                "glass-frida: could not locate `{lib}` via pkg-config: {e}\n\
                 On Debian/Ubuntu install it with `sudo apt-get install libglib2.0-dev`."
            );
        }
    }
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
