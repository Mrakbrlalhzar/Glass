//! Fat Mach-O thinning. Apple ships universal binaries with a small
//! big-endian fat header followed by N `fat_arch` records pointing at
//! per-architecture thin slices. `armv8-encode` only decodes thin
//! arm64, so we slice the file ourselves before handing bytes off.

use anyhow::{anyhow, Result};

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
/// If it's already a thin 64-bit Mach-O, return a copy.
/// Anything else is an error — callers can fall back to the original
/// bytes (e.g. ELF goes through unchanged).
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

    // Prefer arm64e over plain arm64 — it's the native slice on modern
    // Apple silicon, and any iOS app post-iPhoneXS ships it.
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
