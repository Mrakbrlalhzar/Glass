//! Mobile bundle loading: APK (Android) and IPA (iOS).
//!
//! Auto-detects by extension and yields a `Bundle` with the
//! interesting pieces already extracted:
//!   - APK -> classes*.dex (parsed) + native libs by ABI
//!   - IPA -> main Mach-O executable + Info.plist + embedded frameworks

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use glass_arch_arm::Arm64Binary;
use glass_arch_dex::DexBinary;
use smali::android::zip::ApkFile;

pub mod ipa;
pub use ipa::{thin_slice_macho, InfoPlist};

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
    /// Raw on-disk bytes of `AndroidManifest.xml` (binary AXML).
    /// Kept so the editor can decode + re-encode the file in its
    /// original format — `manifest` alone is a typed DOM that
    /// loses formatting and any chunk we don't enumerate.
    /// `None` if the archive had no manifest entry.
    pub manifest_bytes: Option<Vec<u8>>,
    /// Archive path of the manifest inside the APK (always
    /// `AndroidManifest.xml` in practice — kept explicit for
    /// symmetry with `IpaBundle::info_archive_path`).
    pub manifest_archive_path: String,
}

pub struct IpaBundle {
    pub path: PathBuf,
    /// Path within the zip to the `.app` folder, e.g.
    /// `Payload/MyApp.app`. Used as a prefix when looking up siblings.
    pub app_dir: String,
    /// Parsed `Info.plist` — bundle id, executable name, version, …
    pub info: InfoPlist,
    /// Raw on-disk bytes of `Info.plist`. Kept so the editor
    /// can round-trip the file in its original format (binary
    /// or XML) — `info` alone is a typed view that has lost
    /// every key we don't enumerate.
    pub info_bytes: Vec<u8>,
    /// Archive path of the Info.plist inside the IPA, e.g.
    /// `Payload/MyApp.app/Info.plist`. Used by the export path
    /// to splice the edited plist back into the zip.
    pub info_archive_path: String,
    /// Main executable, sliced to arm64/arm64e if the file was fat.
    /// `None` if the executable is missing or armv8-encode couldn't
    /// parse it (e.g. the arm64 slice was absent on an older binary).
    pub main_executable: Option<Arm64Binary>,
    /// Embedded `Frameworks/*.framework` / `*.dylib` metadata.
    /// We list them eagerly but don't disassemble them until the UI
    /// asks — keeps initial open fast.
    pub frameworks: Vec<EmbeddedFramework>,
}

/// Lightweight record for a framework or dylib bundled under
/// `Payload/*.app/Frameworks/`. The binary bytes are kept on the
/// IpaBundle's parent zip; we only carry the in-archive path here.
pub struct EmbeddedFramework {
    /// Display name, e.g. `Foo.framework` or `libBar.dylib`.
    pub name: String,
    /// Full path within the zip to the binary itself.
    pub archive_path: String,
    /// Raw bytes of the binary (already fat-sliced to arm64 if it was
    /// a fat Mach-O). Empty if we couldn't read it.
    pub bytes: Vec<u8>,
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
    let mut manifest_bytes: Option<Vec<u8>> = None;
    if let Some(entry) = apk.entry("AndroidManifest.xml") {
        manifest_bytes = Some(entry.data.clone());
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
        manifest_bytes,
        manifest_archive_path: "AndroidManifest.xml".to_string(),
    })
}

fn open_ipa(path: &Path) -> Result<IpaBundle> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening IPA {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading IPA zip {}", path.display()))?;

    // Locate the .app folder under Payload/. There's always exactly one
    // for App Store builds; enterprise/ad-hoc IPAs follow the same
    // convention.
    let app_dir = find_app_dir(&zip)
        .ok_or_else(|| anyhow!("no Payload/<name>.app/ folder found in IPA"))?;
    tracing::debug!("IPA app dir: {app_dir}");

    // Read Info.plist.
    let info_path = format!("{app_dir}/Info.plist");
    let info_bytes = read_entry(&mut zip, &info_path)
        .with_context(|| format!("reading {info_path}"))?;
    let info = InfoPlist::from_bytes(&info_bytes)
        .with_context(|| format!("parsing {info_path}"))?;

    // Main executable. CFBundleExecutable names it; sits alongside
    // Info.plist. Arm64Binary slices fat Mach-Os transparently.
    let main_executable = match info.executable.as_deref() {
        Some(exec_name) => {
            let exec_path = format!("{app_dir}/{exec_name}");
            match read_entry(&mut zip, &exec_path) {
                Ok(bytes) => match Arm64Binary::from_bytes(PathBuf::from(&exec_path), bytes) {
                    Ok(bin) => Some(bin),
                    Err(e) => {
                        tracing::warn!("main executable parse failed: {e}");
                        None
                    }
                },
                Err(e) => {
                    tracing::warn!("main executable missing ({exec_path}): {e}");
                    None
                }
            }
        }
        None => {
            tracing::warn!("Info.plist has no CFBundleExecutable");
            None
        }
    };

    // Embedded frameworks / dylibs. We read the bytes (and thin-slice)
    // eagerly so the UI can list and later disassemble them, but we
    // don't run armv8-encode on them yet.
    let mut frameworks = Vec::new();
    let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    let frameworks_prefix = format!("{app_dir}/Frameworks/");
    for name in names {
        if !name.starts_with(&frameworks_prefix) {
            continue;
        }
        // .framework/<name> — pick the binary that matches the
        // framework folder name. .dylib — pick the file itself.
        let is_dylib = name.ends_with(".dylib");
        let is_framework_bin = {
            // e.g. "Frameworks/Foo.framework/Foo"
            let rest = &name[frameworks_prefix.len()..];
            match rest.split_once('/') {
                Some((folder, file)) if folder.ends_with(".framework") => {
                    let stem = folder.trim_end_matches(".framework");
                    file == stem
                }
                _ => false,
            }
        };
        if !is_dylib && !is_framework_bin {
            continue;
        }
        let display_name = match name[frameworks_prefix.len()..].split_once('/') {
            Some((folder, _)) => folder.to_string(),
            None => name[frameworks_prefix.len()..].to_string(),
        };
        match read_entry(&mut zip, &name) {
            Ok(bytes) => {
                // Keep the raw bytes — Arm64Binary slices fat headers
                // on parse, and we want the original bytes for hashing
                // / persistence stability.
                frameworks.push(EmbeddedFramework {
                    name: display_name,
                    archive_path: name,
                    bytes,
                });
            }
            Err(e) => {
                tracing::debug!("skipping {name}: {e}");
            }
        }
    }

    Ok(IpaBundle {
        path: path.to_path_buf(),
        app_dir,
        info,
        info_bytes,
        info_archive_path: info_path,
        main_executable,
        frameworks,
    })
}

fn find_app_dir<R: std::io::Read + std::io::Seek>(
    zip: &zip::ZipArchive<R>,
) -> Option<String> {
    for name in zip.file_names() {
        // Looking for `Payload/Foo.app/` — strip trailing slash if it's
        // a directory entry, else find the prefix from a deeper file.
        if let Some(rest) = name.strip_prefix("Payload/") {
            if let Some(idx) = rest.find(".app/") {
                let app_name = &rest[..idx + ".app".len()];
                return Some(format!("Payload/{app_name}"));
            }
            if let Some(stripped) = rest.strip_suffix(".app/") {
                return Some(format!("Payload/{stripped}.app"));
            }
        }
    }
    None
}

fn read_entry<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>> {
    let mut file = zip.by_name(name)
        .with_context(|| format!("zip entry {name} not found"))?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)
        .with_context(|| format!("reading zip entry {name}"))?;
    Ok(buf)
}
