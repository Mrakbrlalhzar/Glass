//! Mobile bundle loading: APK (Android) and IPA (iOS).
//!
//! Auto-detects by magic / extension and yields a `Bundle` with the
//! interesting pieces already extracted:
//!   - APK -> classes*.dex (parsed) + native libs by ABI
//!   - IPA -> main Mach-O executable + embedded frameworks
//!
//! M1 focus is APK. IPA is stubbed.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use glass_arch_arm64::Arm64Binary;
use glass_arch_dex::DexBinary;
use smali::android::zip::ApkFile;

pub enum Bundle {
    Apk(ApkBundle),
    Ipa(IpaBundle),
}

pub struct ApkBundle {
    pub path: PathBuf,
    pub dex_files: Vec<DexBinary>,
    /// Native libs grouped by ABI (e.g. "arm64-v8a", "armeabi-v7a").
    pub native_libs: Vec<NativeLib>,
    /// Parsed AndroidManifest.xml when one was present. APKs always
    /// have one in practice; we leave it `None` if parsing fails so a
    /// broken manifest doesn't block the rest of the bundle from
    /// loading.
    pub manifest: Option<smali::android::binary_xml::AndroidManifest>,
}

pub struct IpaBundle {
    pub path: PathBuf,
    pub main_executable: Option<Arm64Binary>,
    // TODO: embedded Frameworks/*.framework, Info.plist, entitlements.
}

pub struct NativeLib {
    pub abi: String,
    pub name: String,
    pub binary: Arm64Binary,
}

impl Bundle {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match path.extension().and_then(|e| e.to_str()) {
            Some("apk") | Some("aab") => Ok(Bundle::Apk(open_apk(path)?)),
            Some("ipa") => Ok(Bundle::Ipa(open_ipa(path)?)),
            other => Err(anyhow!("unknown bundle extension: {other:?}")),
        }
    }
}

fn open_apk(path: &Path) -> Result<ApkBundle> {
    let apk = ApkFile::from_file(path)
        .with_context(|| format!("opening APK {}", path.display()))?;

    let mut dex_files = Vec::new();
    let mut native_libs = Vec::new();
    let mut manifest = None;
    if let Some(entry) = apk.entry("AndroidManifest.xml") {
        match smali::android::binary_xml::AndroidManifest::from_apk_entry(entry) {
            Ok(m) => manifest = Some(m),
            Err(e) => tracing::warn!("AndroidManifest.xml parse failed: {e}"),
        }
    }

    let names: Vec<String> = apk.entry_names().map(|s| s.to_string()).collect();
    for name in names {
        let entry = match apk.entry(&name) {
            Some(e) => e,
            None => continue,
        };
        if name.ends_with(".dex") {
            dex_files.push(DexBinary::from_bytes(name.clone(), &entry.data)?);
        } else if let Some(rest) = name.strip_prefix("lib/") {
            if !name.ends_with(".so") {
                continue;
            }
            if let Some((abi, lib_name)) = rest.split_once('/') {
                let abi = abi.to_string();
                let lib_name = lib_name.to_string();
                let bytes = entry.data.clone();
                match Arm64Binary::from_bytes(PathBuf::from(&name), bytes) {
                    Ok(binary) => native_libs.push(NativeLib { abi, name: lib_name, binary }),
                    Err(e) => {
                        // Non-AArch64 ABIs (armeabi-v7a, x86, x86_64) will fail
                        // here today — that's fine until we add more arches.
                        tracing::debug!("skipping {name}: {e}");
                    }
                }
            }
        }
    }

    Ok(ApkBundle {
        path: path.to_path_buf(),
        dex_files,
        native_libs,
        manifest,
    })
}

fn open_ipa(path: &Path) -> Result<IpaBundle> {
    // TODO M3: unzip, find Payload/*.app/<exec>, thin-slice if fat, parse
    // Info.plist for executable name + bundle id.
    Ok(IpaBundle {
        path: path.to_path_buf(),
        main_executable: None,
    })
}
