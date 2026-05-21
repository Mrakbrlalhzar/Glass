//! Bundled frida-gadget binaries.
//!
//! Glass ships the gadget for the platforms we can inject into
//! today, baked into the binary via `include_bytes!`. Vendored
//! from frida's GitHub releases (currently 17.9.10) under
//! `crates/glass-frida/assets/gadgets/`:
//!
//!   * `arm64-v8a/libfrida-gadget.so` — Android ARM64 (almost
//!     every modern phone). Injected into `lib/arm64-v8a/`.
//!   * `ios-universal/FridaGadget.dylib` — iOS fat binary
//!     covering arm64 + arm64e + x86_64. Injected into
//!     `Payload/<App>.app/Frameworks/`. iOS injection is a
//!     separate milestone — the binary is here ready for it.
//!
//! Frida-gadget is LGPL-2.1; see the licence note in
//! `crates/glass-ui/src/about.rs` and the upstream Frida repo.

const ANDROID_ARM64: &[u8] =
    include_bytes!("../assets/gadgets/arm64-v8a/libfrida-gadget.so");
const IOS_UNIVERSAL: &[u8] =
    include_bytes!("../assets/gadgets/ios-universal/FridaGadget.dylib");

/// Vendored gadget binary for a given target ABI.
#[derive(Debug, Clone, Copy)]
pub struct GadgetBinary {
    /// Filename to drop into the bundle (`libfrida-gadget.so`
    /// on Android, `FridaGadget.dylib` on iOS).
    pub filename: &'static str,
    /// Raw bytes ready to splice into the APK / IPA.
    pub bytes: &'static [u8],
}

impl GadgetBinary {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// The Android arm64 gadget. Covers ~all phones shipped in the
/// last 5+ years. armeabi-v7a / x86 builds aren't bundled —
/// the planner refuses to inject when those are the only ABIs
/// present.
pub fn android_arm64() -> GadgetBinary {
    GadgetBinary {
        filename: "libfrida-gadget.so",
        bytes: ANDROID_ARM64,
    }
}

/// iOS universal Mach-O. Used by the (future) IPA injection
/// flow. Not consumed by the M3.2a Android planner.
pub fn ios_universal() -> GadgetBinary {
    GadgetBinary {
        filename: "FridaGadget.dylib",
        bytes: IOS_UNIVERSAL,
    }
}

/// Default Frida-gadget config requesting listen mode on the
/// standard host-loopback port. Newer gadget releases (17.x)
/// refuse to operate without a config file alongside them —
/// they log `FATAL: Unable to locate libfrida-gadget.config.so`
/// and bail out. Glass ships this companion so the gadget
/// comes up listening on 127.0.0.1:27042 by default.
///
/// The file is JSON despite the `.so` extension; Frida picks
/// the suffix so Android keeps it uncompressed + page-aligned
/// alongside the actual gadget shared object.
pub const ANDROID_GADGET_CONFIG_FILENAME: &str =
    "libfrida-gadget.config.so";

pub fn android_gadget_config_listen() -> Vec<u8> {
    // Listen on host loopback, fail (rather than auto-retry)
    // if 27042 is taken, and wait for a host-side client
    // before continuing app init. `on_load: wait` is the
    // safer default for a reverse-engineering workflow —
    // gives the user time to attach before the app's own
    // code starts running. Users can override later by
    // writing their own config in the bundle.
    let json = r#"{
  "interaction": {
    "type": "listen",
    "address": "127.0.0.1",
    "port": 27042,
    "on_port_conflict": "fail",
    "on_load": "resume"
  }
}
"#;
    json.as_bytes().to_vec()
}

/// Look up the gadget binary for an Android ABI name. Returns
/// `None` when we don't ship one for that ABI — caller treats
/// that as "can't inject for this device".
pub fn for_android_abi(abi: &str) -> Option<GadgetBinary> {
    match abi {
        "arm64-v8a" => Some(android_arm64()),
        // `armeabi-v7a`, `x86`, `x86_64` could be added later
        // by dropping the matching gadget into assets/gadgets/.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_gadget_is_an_elf() {
        // Sanity-check: the bundled blob is an actual ELF
        // shared object, not an accidental download of the
        // .xz wrapper or an HTML 404 page from GitHub.
        let bytes = android_arm64().bytes;
        assert!(bytes.len() > 1_000_000, "gadget unexpectedly small: {} bytes", bytes.len());
        assert_eq!(&bytes[..4], b"\x7fELF", "android gadget isn't an ELF binary");
    }

    #[test]
    fn ios_gadget_is_macho_fat() {
        // iOS universal gadget is a fat Mach-O. Magic bytes
        // are either MH_MAGIC_64 (0xfeedfacf) or FAT_MAGIC
        // (0xcafebabe), big-endian on disk for fat.
        let bytes = ios_universal().bytes;
        assert!(bytes.len() > 1_000_000);
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert!(
            magic == 0xcafebabe || magic == 0xcafebabf || magic == 0xfeedfacf,
            "unexpected Mach-O magic {:#x}",
            magic,
        );
    }

    #[test]
    fn unknown_android_abi_returns_none() {
        assert!(for_android_abi("mips64").is_none());
        assert!(for_android_abi("armeabi-v7a").is_none());
    }

    #[test]
    fn known_abi_resolves() {
        let g = for_android_abi("arm64-v8a").expect("arm64 gadget bundled");
        assert_eq!(g.filename, "libfrida-gadget.so");
    }
}
