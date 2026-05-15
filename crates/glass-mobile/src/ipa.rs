//! IPA-specific helpers: Info.plist parsing.
//!
//! The `plist` crate handles both binary and XML plists transparently,
//! so we don't have to care which encoding Xcode chose.
//!
//! Fat Mach-O slicing lives in `glass-arch-arm64::macho_fat` so every
//! caller of `Arm64Binary::from_bytes` benefits transparently. We
//! re-export it here for the legacy `glass_mobile::thin_slice_macho`
//! path.

use anyhow::{Context, Result};
use serde::Deserialize;

pub use glass_arch_arm64::thin_slice_macho;

/// Subset of `Info.plist` fields we currently surface in the UI.
/// Anything we don't model here is still available via `extras` for
/// scripting / future passes.
#[derive(Debug, Clone, Default)]
pub struct InfoPlist {
    /// `CFBundleIdentifier`, e.g. `com.example.Foo`.
    pub bundle_id: Option<String>,
    /// `CFBundleExecutable` — name of the Mach-O alongside Info.plist.
    pub executable: Option<String>,
    /// `CFBundleDisplayName` (preferred) or `CFBundleName`.
    pub display_name: Option<String>,
    /// `CFBundleShortVersionString`, e.g. `1.2.3`.
    pub short_version: Option<String>,
    /// `CFBundleVersion`, the build number.
    pub build_version: Option<String>,
    /// `MinimumOSVersion`.
    pub min_os: Option<String>,
    /// `DTPlatformName`, usually `iphoneos`.
    pub platform: Option<String>,
    /// Everything else, kept as a generic plist value so scripts can
    /// dig in without us having to model every Apple key.
    pub extras: Option<plist::Value>,
}

#[derive(Deserialize)]
struct RawInfo {
    #[serde(rename = "CFBundleIdentifier")]
    bundle_id: Option<String>,
    #[serde(rename = "CFBundleExecutable")]
    executable: Option<String>,
    #[serde(rename = "CFBundleDisplayName")]
    display_name: Option<String>,
    #[serde(rename = "CFBundleName")]
    fallback_name: Option<String>,
    #[serde(rename = "CFBundleShortVersionString")]
    short_version: Option<String>,
    #[serde(rename = "CFBundleVersion")]
    build_version: Option<String>,
    #[serde(rename = "MinimumOSVersion")]
    min_os: Option<String>,
    #[serde(rename = "DTPlatformName")]
    platform: Option<String>,
}

impl InfoPlist {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let raw: RawInfo = plist::from_bytes(bytes)
            .context("parsing Info.plist")?;
        let extras = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok();
        Ok(InfoPlist {
            bundle_id: raw.bundle_id,
            executable: raw.executable,
            display_name: raw.display_name.or(raw.fallback_name),
            short_version: raw.short_version,
            build_version: raw.build_version,
            min_os: raw.min_os,
            platform: raw.platform,
            extras,
        })
    }
}

