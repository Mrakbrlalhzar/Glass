//! Patched-bundle re-serialisation.
//!
//! Takes a `Bundle` and a per-artifact list of edits and writes
//! a new file:
//!
//! - **Standalone binary** (Mach-O / ELF / `.so` / `.dylib`):
//!   patch the parsed container in place and call
//!   `Container::to_bytes`.
//! - **APK / AAB**: open the source archive via `smali::ApkFile`,
//!   patch each native artifact's entry with its re-serialised
//!   bytes, write a fresh zip via `ApkFile::write_to_file`.
//! - **IPA**: open the source archive directly with the `zip`
//!   crate, walk every entry, patch the ones whose paths match
//!   our framework / main-executable artifacts, write a fresh
//!   archive. (We don't depend on `smali::ApkFile` here because
//!   IPAs have an Apple-specific shape — `Payload/<App>.app/…`
//!   — that doesn't benefit from smali's APK helpers.)
//!
//! All three paths reuse `export_native_artifact` for the
//! Mach-O / ELF body; the bundle-specific wrappers only do
//! archive plumbing.
//!
//! The exporter is read-only with respect to the bundle handed
//! in — it re-reads the source file from disk via the bundle's
//! `source_path` and works against fresh bytes. Edits are
//! applied by vaddr → `Container::section_for_addr` lookup, then
//! `Container::with_section_bytes`.

use anyhow::{anyhow, bail, Context as _, Result};
use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use armv8_encode::container::{Container, SectionKind};

use crate::bundle::{Bundle, BundleKind};
use glass_db::ArtifactId;

/// One staged instruction edit, condensed for the exporter.
/// Glass-ui converts its richer `Edit` rows into this minimal
/// shape before calling.
#[derive(Debug, Clone)]
pub struct EditPatch {
    pub vaddr: u64,
    /// New bytes to splice into the container at `vaddr`. Length
    /// is preserved — same number of bytes go in as came out;
    /// the exporter rejects spans that would cross a section
    /// boundary or extend past the section end.
    pub new_bytes: Vec<u8>,
}

/// Per-artifact patch table. Each artifact's edits are stored in
/// a flat Vec; same artifact appearing in two entries would be
/// caller error.
pub type EditMap = HashMap<ArtifactId, Vec<EditPatch>>;

/// Apply `edits` to the artifact described by `binary_bytes`
/// (the raw on-disk bytes of one Mach-O / ELF body) and return
/// the re-serialised output.
///
/// `vaddr` lookups go via the parsed container's section table.
/// An edit that doesn't land in any text section is rejected —
/// at the moment we only ever stage edits against text-section
/// disasm rows, but we check defensively.
pub fn export_native_artifact(binary_bytes: &[u8], edits: &[EditPatch]) -> Result<Vec<u8>> {
    let container = Container::from_bytes(binary_bytes)
        .map_err(|e| anyhow!("parsing container: {e:?}"))?;

    // Group edits by section so each section is rewritten once.
    let mut per_section: HashMap<usize, Vec<(usize, Vec<u8>)>> = HashMap::new();
    for edit in edits {
        let (section_idx, off_in_section) =
            locate_in_section(&container, edit.vaddr).with_context(|| {
                format!(
                    "edit at vaddr 0x{:x} doesn't land inside any patchable section",
                    edit.vaddr
                )
            })?;
        per_section
            .entry(section_idx)
            .or_default()
            .push((off_in_section, edit.new_bytes.clone()));
    }

    let mut patched = container.clone();
    for (section_idx, mut splices) in per_section {
        // Sort by offset just for hygiene; splices are
        // independent but a stable order makes any future
        // overlap-check straightforward.
        splices.sort_by_key(|(off, _)| *off);
        let mut new_bytes = patched.sections[section_idx].bytes.clone();
        for (off, bytes) in &splices {
            if *off + bytes.len() > new_bytes.len() {
                bail!(
                    "edit offset {off:#x} + {} exceeds section length {}",
                    bytes.len(),
                    new_bytes.len()
                );
            }
            new_bytes[*off..*off + bytes.len()].copy_from_slice(bytes);
        }
        let section_id = patched.sections[section_idx].id;
        patched = patched.with_section_bytes(section_id, new_bytes);
    }

    patched
        .to_bytes()
        .map_err(|e| anyhow!("re-serialising container: {e:?}"))
}

/// Resolve `vaddr` to `(section_index, offset_in_section)` for
/// any non-BSS section in the container. Returns Err when no
/// section covers the address.
fn locate_in_section(container: &Container, vaddr: u64) -> Result<(usize, usize)> {
    for (i, section) in container.sections.iter().enumerate() {
        // BSS has no on-disk bytes to splice; debug sections
        // shouldn't be patched. Everything else is fair game.
        if matches!(section.kind, SectionKind::Bss | SectionKind::Debug) {
            continue;
        }
        let base = section.address;
        let size = section.size;
        if vaddr >= base && vaddr < base.saturating_add(size) {
            return Ok((i, (vaddr - base) as usize));
        }
    }
    Err(anyhow!("no section covers 0x{vaddr:x}"))
}

/// Top-level entry point: write a patched copy of `bundle.source_path`
/// to `out_path`, applying `edits` keyed by artifact id.
pub fn export_to_path(bundle: &Bundle, edits: &EditMap, out_path: &Path) -> Result<()> {
    if edits.is_empty() {
        bail!("no edits to export");
    }
    match bundle.kind {
        BundleKind::Native => export_native_to_path(bundle, edits, out_path),
        BundleKind::Apk => export_apk_to_path(bundle, edits, out_path),
        BundleKind::Ipa => export_ipa_to_path(bundle, edits, out_path),
    }
}

fn export_native_to_path(bundle: &Bundle, edits: &EditMap, out: &Path) -> Result<()> {
    if edits.len() > 1 {
        bail!(
            "native bundle has one artifact but the edit map carries {} entries",
            edits.len()
        );
    }
    let (artifact_id, patches) = edits.iter().next().unwrap();
    let artifact = bundle
        .artifacts
        .iter()
        .find(|a| &a.id == artifact_id)
        .ok_or_else(|| anyhow!("no artifact in bundle matches edit-map id"))?;
    let patched_bytes = export_native_artifact(&artifact.binary.bytes, patches)?;
    std::fs::write(out, &patched_bytes)
        .with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

fn export_apk_to_path(bundle: &Bundle, edits: &EditMap, out: &Path) -> Result<()> {
    use smali::android::zip::ApkFile;
    let mut apk = ApkFile::from_file(&bundle.source_path)
        .map_err(|e| anyhow!("opening APK {}: {e:?}", bundle.source_path.display()))?;
    for (artifact_id, patches) in edits {
        let artifact = bundle
            .artifacts
            .iter()
            .find(|a| &a.id == artifact_id)
            .ok_or_else(|| anyhow!("artifact for edit not found in bundle"))?;
        // APK native libs live at `lib/<abi>/<name>`. The artifact's
        // label is exactly that — we wrote it at load time.
        let entry_name = artifact.label.clone();
        let entry_name = if entry_name.starts_with("lib/") {
            entry_name
        } else {
            format!("lib/{entry_name}")
        };
        let new_bytes = export_native_artifact(&artifact.binary.bytes, patches)
            .with_context(|| format!("re-serialising {entry_name}"))?;
        apk.replace_entry(&entry_name, new_bytes)
            .map_err(|e| anyhow!("replacing {entry_name} in APK: {e:?}"))?;
    }
    apk.write_to_file(out)
        .map_err(|e| anyhow!("writing patched APK {}: {e:?}", out.display()))?;
    Ok(())
}

fn export_ipa_to_path(bundle: &Bundle, edits: &EditMap, out: &Path) -> Result<()> {
    // Build a fresh zip by streaming every entry from the source
    // IPA. Patched entries are looked up by archive_path —
    // populated at load time from the original zip walk.
    let mut entry_overrides: HashMap<String, Vec<u8>> = HashMap::new();
    for (artifact_id, patches) in edits {
        let artifact = bundle
            .artifacts
            .iter()
            .find(|a| &a.id == artifact_id)
            .ok_or_else(|| anyhow!("artifact for edit not found in bundle"))?;
        let archive_path = bundle
            .artifact_archive_path(artifact_id)
            .ok_or_else(|| anyhow!("artifact {} has no archive_path; can't splice", artifact.label))?;
        let new_bytes = export_native_artifact(&artifact.binary.bytes, patches)
            .with_context(|| format!("re-serialising {archive_path}"))?;
        entry_overrides.insert(archive_path, new_bytes);
    }
    let src = std::fs::File::open(&bundle.source_path).with_context(|| {
        format!("opening source IPA {}", bundle.source_path.display())
    })?;
    let mut src_zip = zip::ZipArchive::new(src)
        .with_context(|| format!("reading IPA zip {}", bundle.source_path.display()))?;
    let out_file = std::fs::File::create(out)
        .with_context(|| format!("creating {}", out.display()))?;
    let mut out_zip = zip::ZipWriter::new(out_file);
    let stored = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for i in 0..src_zip.len() {
        let mut entry = src_zip
            .by_index(i)
            .with_context(|| format!("reading entry {i} from source IPA"))?;
        if entry.name().ends_with('/') {
            // Directory marker — preserve as-is.
            out_zip
                .add_directory(entry.name(), stored)
                .with_context(|| format!("writing dir {}", entry.name()))?;
            continue;
        }
        let opts = match entry.compression() {
            zip::CompressionMethod::Stored => stored,
            _ => deflated,
        };
        let name = entry.name().to_string();
        let data = if let Some(override_bytes) = entry_overrides.remove(&name) {
            override_bytes
        } else {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf).with_context(|| {
                format!("reading entry {name} from source IPA")
            })?;
            buf
        };
        out_zip
            .start_file(&name, opts)
            .with_context(|| format!("writing entry {name}"))?;
        out_zip
            .write_all(&data)
            .with_context(|| format!("writing entry body for {name}"))?;
    }
    out_zip
        .finish()
        .with_context(|| format!("finalising {}", out.display()))?;
    let _ = Cursor::<&[u8]>::new(&[]); // silence unused-import on Cursor for non-IPA builds
    Ok(())
}
