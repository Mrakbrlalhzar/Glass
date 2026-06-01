//! Download + push + launch a matching `frida-server` binary on
//! a rooted Android device.
//!
//! The pinned Frida release version (read at build time from
//! `frida-sys`'s `FRIDA_VERSION` file — see `build.rs`) drives
//! the URL we fetch from GitHub. The downloaded `.xz` tarball is
//! cached under `~/Library/Caches/glass/frida-server/` so
//! subsequent installs on more devices skip the network.
//!
//! This module is host-side only — it knows about decompression
//! and cache paths but not about `adb`. The device-side push +
//! exec lives in `glass-device::adb` so we don't drag adb
//! orchestration into the Frida driver.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Frida release version this build of Glass links against.
/// Baked in by `build.rs`. "unknown" when the `frida` feature is
/// off (we can't push a server we don't know the version of).
pub const FRIDA_VERSION: &str = env!("GLASS_FRIDA_VERSION");

/// Frida's Android server binaries. Names mirror the suffix on
/// the GitHub release assets — `frida-server-<ver>-android-<arch>.xz`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AndroidServerArch {
    Arm64,
    Arm,
    X86_64,
    X86,
}

impl AndroidServerArch {
    /// Map the value of `getprop ro.product.cpu.abi` to a
    /// release-asset arch slug. Returns `None` for ABIs Frida
    /// doesn't publish a binary for — caller surfaces a clear
    /// error rather than guessing.
    pub fn from_abi(abi: &str) -> Option<Self> {
        match abi.trim() {
            "arm64-v8a" => Some(Self::Arm64),
            "armeabi-v7a" | "armeabi" => Some(Self::Arm),
            "x86_64" => Some(Self::X86_64),
            "x86" => Some(Self::X86),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::Arm => "arm",
            Self::X86_64 => "x86_64",
            Self::X86 => "x86",
        }
    }
}

/// Errors raised when staging frida-server. Cloneable so the GUI
/// can stash one in its install-state model.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ServerStageError {
    #[error("Frida version unknown — build.rs failed to read FRIDA_VERSION from frida-sys's checkout")]
    VersionUnknown,
    #[error("device ABI {0:?} has no published frida-server binary")]
    UnsupportedAbi(String),
    #[error("creating cache dir {path}: {cause}")]
    CacheDir { path: String, cause: String },
    #[error("HTTP error fetching {url}: {cause}")]
    Http { url: String, cause: String },
    #[error("decompressing {path}: {cause}")]
    Decompress { path: String, cause: String },
    #[error("writing {path}: {cause}")]
    Write { path: String, cause: String },
}

/// Path layout (host-side cache, not the path on the device):
/// ```text
/// ~/Library/Caches/glass/frida-server/
///   ├── 17.9.5/
///   │   ├── frida-server-17.9.5-android-arm64.xz   (raw download, kept)
///   │   └── frida-server-17.9.5-android-arm64       (decompressed, pushed)
/// ```
pub fn cache_root() -> Result<PathBuf, ServerStageError> {
    let base = dirs::cache_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("glass").join("frida-server").join(FRIDA_VERSION);
    fs::create_dir_all(&dir).map_err(|e| ServerStageError::CacheDir {
        path: dir.display().to_string(),
        cause: e.to_string(),
    })?;
    Ok(dir)
}

/// Asset URL for a given arch under the pinned Frida version.
pub fn asset_url(arch: AndroidServerArch) -> String {
    format!(
        "https://github.com/frida/frida/releases/download/{ver}/frida-server-{ver}-android-{arch}.xz",
        ver = FRIDA_VERSION,
        arch = arch.slug(),
    )
}

/// Asset name on disk inside the cache, for both the `.xz` and
/// the decompressed binary (without the suffix).
pub fn asset_name(arch: AndroidServerArch) -> String {
    format!(
        "frida-server-{ver}-android-{arch}",
        ver = FRIDA_VERSION,
        arch = arch.slug(),
    )
}

/// Stage progress phases — surfaced to the UI as a status line.
#[derive(Clone, Debug)]
pub enum StageProgress {
    /// Cache already has a matching binary; we'll skip the
    /// download.
    CacheHit,
    /// Downloading the `.xz` from GitHub. `bytes_so_far` and the
    /// optional `total` together drive a progress bar.
    Downloading { bytes_so_far: u64, total: Option<u64> },
    /// Decompressing the cached tarball.
    Decompressing,
    /// Finished — the path is the decompressed binary the caller
    /// can `adb push`.
    Ready(PathBuf),
}

/// Download (if needed) and decompress the frida-server binary
/// matching the host devkit version. Blocking; expects to be
/// called from a worker thread. Progress is reported through the
/// `on_progress` callback so the GUI can update the dialog
/// without polling.
pub fn stage_server<F>(
    arch: AndroidServerArch,
    mut on_progress: F,
) -> Result<PathBuf, ServerStageError>
where
    F: FnMut(StageProgress),
{
    if FRIDA_VERSION == "unknown" {
        return Err(ServerStageError::VersionUnknown);
    }
    let dir = cache_root()?;
    let name = asset_name(arch);
    let xz_path = dir.join(format!("{name}.xz"));
    let bin_path = dir.join(&name);
    if bin_path.exists() {
        on_progress(StageProgress::CacheHit);
        on_progress(StageProgress::Ready(bin_path.clone()));
        return Ok(bin_path);
    }
    if !xz_path.exists() {
        let url = asset_url(arch);
        download(&url, &xz_path, &mut on_progress)?;
    }
    on_progress(StageProgress::Decompressing);
    decompress_xz(&xz_path, &bin_path)?;
    on_progress(StageProgress::Ready(bin_path.clone()));
    Ok(bin_path)
}

fn download<F>(
    url: &str,
    out_path: &Path,
    on_progress: &mut F,
) -> Result<(), ServerStageError>
where
    F: FnMut(StageProgress),
{
    // Use a generous timeout — the GitHub redirect can take a
    // moment, and the body itself can run to 5+ MB.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(120))
        .build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| ServerStageError::Http {
            url: url.to_string(),
            cause: e.to_string(),
        })?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok());
    let tmp = out_path.with_extension("xz.part");
    let mut file = fs::File::create(&tmp).map_err(|e| ServerStageError::Write {
        path: tmp.display().to_string(),
        cause: e.to_string(),
    })?;
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut bytes_so_far: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|e| ServerStageError::Http {
            url: url.to_string(),
            cause: e.to_string(),
        })?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n]).map_err(|e| {
            ServerStageError::Write {
                path: tmp.display().to_string(),
                cause: e.to_string(),
            }
        })?;
        bytes_so_far += n as u64;
        on_progress(StageProgress::Downloading { bytes_so_far, total });
    }
    drop(file);
    fs::rename(&tmp, out_path).map_err(|e| ServerStageError::Write {
        path: out_path.display().to_string(),
        cause: e.to_string(),
    })?;
    Ok(())
}

fn decompress_xz(
    xz_path: &Path,
    out_path: &Path,
) -> Result<(), ServerStageError> {
    let raw = fs::read(xz_path).map_err(|e| ServerStageError::Decompress {
        path: xz_path.display().to_string(),
        cause: e.to_string(),
    })?;
    let mut cursor = std::io::Cursor::new(raw);
    let mut decompressed = Vec::new();
    lzma_rs::xz_decompress(&mut cursor, &mut decompressed).map_err(|e| {
        ServerStageError::Decompress {
            path: xz_path.display().to_string(),
            cause: e.to_string(),
        }
    })?;
    let tmp = out_path.with_extension("part");
    fs::write(&tmp, &decompressed).map_err(|e| ServerStageError::Write {
        path: tmp.display().to_string(),
        cause: e.to_string(),
    })?;
    fs::rename(&tmp, out_path).map_err(|e| ServerStageError::Write {
        path: out_path.display().to_string(),
        cause: e.to_string(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_mapping_covers_common_androids() {
        assert_eq!(
            AndroidServerArch::from_abi("arm64-v8a"),
            Some(AndroidServerArch::Arm64),
        );
        assert_eq!(
            AndroidServerArch::from_abi("armeabi-v7a"),
            Some(AndroidServerArch::Arm),
        );
        assert_eq!(
            AndroidServerArch::from_abi("x86_64"),
            Some(AndroidServerArch::X86_64),
        );
        assert_eq!(AndroidServerArch::from_abi("x86"), Some(AndroidServerArch::X86));
        assert_eq!(AndroidServerArch::from_abi("mips"), None);
    }

    #[test]
    fn asset_url_format_matches_frida_releases() {
        // Don't assert on FRIDA_VERSION itself (it floats with
        // upstream bumps), just on the URL shape.
        let url = asset_url(AndroidServerArch::Arm64);
        assert!(
            url.starts_with("https://github.com/frida/frida/releases/download/"),
            "url = {url}",
        );
        assert!(url.ends_with("-android-arm64.xz"), "url = {url}");
    }

    #[test]
    fn version_is_resolved_at_build_time() {
        // build.rs must have read a real semver out of the
        // frida-sys checkout. "unknown" here means the
        // build script's fallback fired and we'd silently
        // fail to download a server.
        assert_ne!(
            FRIDA_VERSION, "unknown",
            "build.rs failed to locate frida-sys FRIDA_VERSION",
        );
        // Sanity: looks like `<major>.<minor>.<patch>`.
        assert_eq!(
            FRIDA_VERSION.split('.').count(),
            3,
            "unexpected FRIDA_VERSION shape: {FRIDA_VERSION}",
        );
    }

    #[test]
    fn asset_name_has_no_extension() {
        let name = asset_name(AndroidServerArch::Arm64);
        assert!(name.ends_with("-android-arm64"));
        assert!(!name.ends_with(".xz"));
    }
}
