//! The `Bundle` handle.
//!
//! Wraps the parsed artifact set + lazy per-query indices. Opened
//! via [`open`]; queries hang off `&Bundle`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use glass_arch_arm64::{Arm64Binary, SymbolMap};
use glass_db::ArtifactId;
use parking_lot::RwLock;
use smali::types::SmaliClass;

/// What kind of input a bundle came from. Drives which artifact
/// fields are populated and which queries are meaningful.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleKind {
    /// Android `.apk` / `.aab` — may contain DEX files and native libs.
    Apk,
    /// iOS `.ipa` — main executable + frameworks/dylibs.
    Ipa,
    /// Bare ELF / Mach-O / fat Mach-O (`.so`, `.dylib`, no-ext executables).
    Native,
}

/// Per-artifact data we hold once the bundle is parsed. One entry
/// per native lib (APK `lib/<abi>/*.so`, IPA main exec + frameworks,
/// or the single artifact for a standalone binary).
///
/// `Arm64Binary` doesn't implement Clone (the underlying container
/// owns megabyte-scale section bytes); we wrap it in Arc so the
/// handle is cheap to share with worker threads.
#[allow(dead_code)] // symbol_map populated by the symbols verbs
                    // (task #79); kept ready for the wiring.
pub(crate) struct ParsedArtifact {
    pub id: ArtifactId,
    pub label: String,
    pub binary: Arc<Arm64Binary>,
    /// Built lazily; first symbol-query fills this.
    pub symbol_map: RwLock<Option<Arc<SymbolMap>>>,
    /// Source archive path inside the parent bundle, when one
    /// applies. APK native libs: `lib/<abi>/<name>`. IPA
    /// frameworks: full zip entry path. `None` for standalone
    /// native artifacts (the bundle's `source_path` is the
    /// artifact). The exporter uses this to splice patched bytes
    /// back into the originating archive entry.
    pub archive_path: Option<String>,
}

/// An opened bundle plus its lazy caches.
pub struct Bundle {
    pub(crate) source_path: PathBuf,
    pub(crate) kind: BundleKind,
    pub(crate) label: String,
    pub(crate) artifacts: Vec<ParsedArtifact>,
    /// Lifted DEX classes (APK only). Aggregated across every
    /// `classes*.dex` file in the bundle. Empty for IPA / Native.
    pub(crate) dex_classes: Vec<SmaliClass>,
    // Future caches (search index, xref maps) hang off this struct
    // as more verbs land — each behind its own RwLock<Option<Arc<...>>>
    // so cache fills serialise without blocking the reader path.
}

impl Bundle {
    /// File / directory the bundle was loaded from.
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn kind(&self) -> BundleKind {
        self.kind
    }

    /// Human-readable label — typically the bundle's filename.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// All artifact ids in load order.
    pub fn artifact_ids(&self) -> impl Iterator<Item = &ArtifactId> {
        self.artifacts.iter().map(|a| &a.id)
    }

    /// Look up an artifact by id. Returns the parsed record if
    /// present. Used by symbol / disasm verbs in follow-up tasks.
    #[allow(dead_code)]
    pub(crate) fn artifact_by_id(&self, id: &ArtifactId) -> Option<&ParsedArtifact> {
        self.artifacts.iter().find(|a| &a.id == id)
    }

    /// Source-archive entry path for an artifact, if one applies.
    /// See `ParsedArtifact::archive_path` for the per-bundle-kind
    /// semantics.
    pub fn artifact_archive_path(&self, id: &ArtifactId) -> Option<String> {
        self.artifacts
            .iter()
            .find(|a| &a.id == id)
            .and_then(|a| a.archive_path.clone())
    }

    /// Look up an artifact by label (case-sensitive exact match)
    /// or by hex-prefix of its `ArtifactId`. Lets CLI users pass
    /// `--artifact libfoo.so` or `--artifact abc123` without
    /// needing the full 64-char hash.
    pub fn resolve_artifact(&self, needle: &str) -> Option<&ArtifactId> {
        for a in &self.artifacts {
            if a.label == needle {
                return Some(&a.id);
            }
            if a.id.to_string().starts_with(needle) {
                return Some(&a.id);
            }
        }
        None
    }
}

/// Open a bundle, parsing artifacts but deferring index builds.
///
/// Recognises APK / AAB / IPA archives and bare ELF / Mach-O
/// binaries (fat Mach-Os are sliced to arm64 / arm64e
/// transparently). Returns an error for unrecognised inputs.
pub fn open(path: impl AsRef<Path>) -> Result<Bundle> {
    let path = path.as_ref().to_path_buf();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let label = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bundle")
        .to_string();

    if matches!(ext.as_str(), "apk" | "aab") {
        open_apk(&path, label)
    } else if ext == "ipa" {
        open_ipa(&path, label)
    } else {
        open_native(&path, label)
    }
}

fn open_apk(path: &Path, label: String) -> Result<Bundle> {
    let apk = match glass_mobile::Bundle::open(path).context("opening APK")? {
        glass_mobile::Bundle::Apk(a) => a,
        _ => anyhow::bail!("expected APK at {}", path.display()),
    };
    let mut artifacts = Vec::with_capacity(apk.native_libs.len());
    for lib in apk.native_libs {
        let id = ArtifactId::from_bytes(&lib.binary.bytes);
        let label = format!("{}/{}", lib.abi, lib.name);
        let archive_path = format!("lib/{label}");
        artifacts.push(ParsedArtifact {
            id,
            label,
            binary: Arc::new(lib.binary),
            symbol_map: RwLock::new(None),
            archive_path: Some(archive_path),
        });
    }
    let mut dex_classes = Vec::new();
    for dex in &apk.dex_files {
        let classes = dex
            .classes()
            .with_context(|| format!("lifting smali from {}", dex.name))?;
        dex_classes.extend(classes.iter().cloned());
    }
    Ok(Bundle {
        source_path: path.to_path_buf(),
        kind: BundleKind::Apk,
        label,
        artifacts,
        dex_classes,
    })
}

fn open_ipa(path: &Path, label: String) -> Result<Bundle> {
    let ipa = match glass_mobile::Bundle::open(path).context("opening IPA")? {
        glass_mobile::Bundle::Ipa(i) => i,
        _ => anyhow::bail!("expected IPA at {}", path.display()),
    };
    let main_label = ipa
        .info
        .executable
        .clone()
        .unwrap_or_else(|| "main".to_string());
    let mut artifacts = Vec::new();
    if let Some(bin) = ipa.main_executable {
        let id = ArtifactId::from_bytes(&bin.bytes);
        // Arm64Binary.path is the zip entry path the main exec
        // was loaded from (set during the IPA walk).
        let archive_path = bin.path.to_string_lossy().to_string();
        artifacts.push(ParsedArtifact {
            id,
            label: main_label,
            binary: Arc::new(bin),
            symbol_map: RwLock::new(None),
            archive_path: Some(archive_path),
        });
    }
    for fw in ipa.frameworks {
        let id = ArtifactId::from_bytes(&fw.bytes);
        let archive_path = fw.archive_path.clone();
        // Frameworks ship the binary bytes pre-extracted; the
        // Arm64Binary needs an explicit construction. For now we
        // re-open from the unsliced bytes — fat binaries handled
        // internally by Arm64Binary::from_bytes.
        let binary = Arm64Binary::from_bytes(
            std::path::PathBuf::from(&fw.archive_path),
            fw.bytes,
        )
        .with_context(|| format!("parsing framework {}", fw.name))?;
        artifacts.push(ParsedArtifact {
            id,
            label: fw.name,
            binary: Arc::new(binary),
            symbol_map: RwLock::new(None),
            archive_path: Some(archive_path),
        });
    }
    Ok(Bundle {
        source_path: path.to_path_buf(),
        kind: BundleKind::Ipa,
        label,
        artifacts,
        dex_classes: Vec::new(),
    })
}

fn open_native(path: &Path, label: String) -> Result<Bundle> {
    let binary = Arm64Binary::open(path)
        .with_context(|| format!("opening native binary {}", path.display()))?;
    let id = ArtifactId::from_bytes(&binary.bytes);
    let artifact = ParsedArtifact {
        id,
        label: label.clone(),
        binary: Arc::new(binary),
        archive_path: None,
        symbol_map: RwLock::new(None),
    };
    Ok(Bundle {
        source_path: path.to_path_buf(),
        kind: BundleKind::Native,
        label,
        artifacts: vec![artifact],
        dex_classes: Vec::new(),
    })
}
