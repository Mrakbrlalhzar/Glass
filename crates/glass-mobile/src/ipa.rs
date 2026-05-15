//! IPA-specific helpers: Info.plist parsing and fat Mach-O slicing.
//!
//! The `plist` crate handles both binary and XML plists transparently,
//! so we don't have to care which encoding Xcode chose.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

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

// Mach-O magics. Apple stores fat headers big-endian regardless of
// host byte order, hence the two forms.
const FAT_MAGIC: u32 = 0xCAFEBABE;
const FAT_CIGAM: u32 = 0xBEBAFECA;
const FAT_MAGIC_64: u32 = 0xCAFEBABF;
const FAT_CIGAM_64: u32 = 0xBFBAFECA;
const MH_MAGIC_64: u32 = 0xFEEDFACF;
const MH_CIGAM_64: u32 = 0xCFFAEDFE;

const CPU_TYPE_ARM64: u32 = 0x0100_000C;
const CPU_SUBTYPE_ARM64E: u32 = 2;

/// If `bytes` is a fat Mach-O, return the arm64/arm64e slice.
/// If it's already a thin Mach-O, return a copy.
/// Anything else is an error.
pub fn thin_slice_macho(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < 4 {
        return Err(anyhow!("file too small to be Mach-O"));
    }
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    match magic {
        MH_MAGIC_64 | MH_CIGAM_64 => Ok(bytes.to_vec()),
        FAT_MAGIC | FAT_CIGAM => slice_fat(bytes, false),
        FAT_MAGIC_64 | FAT_CIGAM_64 => slice_fat(bytes, true),
        _ => Err(anyhow!("not a Mach-O (magic 0x{magic:08x})")),
    }
}

fn slice_fat(bytes: &[u8], is_64: bool) -> Result<Vec<u8>> {
    // Fat headers are always big-endian.
    let read_be_u32 = |off: usize| -> Result<u32> {
        let s = bytes.get(off..off + 4).ok_or_else(|| anyhow!("fat: short read at {off}"))?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    let read_be_u64 = |off: usize| -> Result<u64> {
        let s = bytes.get(off..off + 8).ok_or_else(|| anyhow!("fat: short read at {off}"))?;
        Ok(u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    };
    let nfat = read_be_u32(4)?;
    let arch_size = if is_64 { 32 } else { 20 };
    let arches_off = 8;

    // First pass: find arm64e, then arm64. arm64e gets priority since
    // it's the native slice on modern devices and any apple-silicon
    // iOS app shipped post-iPhoneXS will have it.
    let mut chosen: Option<(u64, u64)> = None; // (offset, size)
    let mut chosen_is_arm64e = false;
    for i in 0..(nfat as usize) {
        let base = arches_off + i * arch_size;
        let cputype = read_be_u32(base)?;
        let cpusubtype = read_be_u32(base + 4)?;
        if cputype != CPU_TYPE_ARM64 {
            continue;
        }
        let (offset, size) = if is_64 {
            (read_be_u64(base + 8)?, read_be_u64(base + 16)?)
        } else {
            (read_be_u32(base + 8)? as u64, read_be_u32(base + 12)? as u64)
        };
        let is_arm64e = (cpusubtype & 0x00FF_FFFF) == CPU_SUBTYPE_ARM64E;
        if chosen.is_none() || (is_arm64e && !chosen_is_arm64e) {
            chosen = Some((offset, size));
            chosen_is_arm64e = is_arm64e;
        }
    }
    let (offset, size) = chosen
        .ok_or_else(|| anyhow!("fat Mach-O contains no arm64 slice"))?;
    let off = offset as usize;
    let end = off
        .checked_add(size as usize)
        .ok_or_else(|| anyhow!("fat: arch range overflow"))?;
    let slice = bytes
        .get(off..end)
        .ok_or_else(|| anyhow!("fat: arch range out of bounds"))?;
    Ok(slice.to_vec())
}
