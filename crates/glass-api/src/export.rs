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

/// Per-DEX typed smali-class edits. Outer key is the DEX
/// artifact id (blake3 of the raw `classes*.dex` bytes — matches
/// what the loader hashes at open time). Inner key is the class
/// JNI signature (`Lcom/foo/Bar;`); value is the rewritten
/// SmaliClass that replaces the original in the re-emitted DEX.
pub type SmaliEditMap =
    HashMap<ArtifactId, HashMap<String, smali::types::SmaliClass>>;

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
/// to `out_path`, applying byte-level `edits` keyed by artifact id.
///
/// Use [`export_to_path_with_smali`] when there are also typed
/// smali-class edits to re-emit. This wrapper is kept for the
/// byte-only callers (CLI `export-patched`, tests).
pub fn export_to_path(bundle: &Bundle, edits: &EditMap, out_path: &Path) -> Result<()> {
    let empty_smali = SmaliEditMap::new();
    let empty_adds: ApkAdditions = std::collections::BTreeMap::new();
    let empty_plist: PlistEditMap = std::collections::BTreeMap::new();
    export_to_path_full(
        bundle, edits, &empty_smali, &empty_adds, &empty_plist, out_path,
    )
}

/// Brand-new entries to splice into an APK at export time.
/// Keyed by their zip-entry path
/// (`lib/arm64-v8a/libfrida-gadget.so`, etc.). The export pipe
/// adds these alongside any replacements from `edits` /
/// `smali_edits`. Only meaningful for APK bundles.
pub type ApkAdditions = std::collections::BTreeMap<String, Vec<u8>>;

/// Whole-plist replacements keyed by zip-entry archive path
/// (e.g. `Payload/MyApp.app/Info.plist`). Each value is the
/// serialised bytes the editor produced — already in the
/// original on-disk format (binary or XML), ready to splice
/// in. Only meaningful for IPA bundles.
pub type PlistEditMap = std::collections::BTreeMap<String, Vec<u8>>;

/// APK-only convenience: edits + smali edits + additions, no
/// plist edits. Kept for back-compat with existing call sites;
/// new IPA-aware callers should use `export_to_path_full`.
pub fn export_to_path_with_smali(
    bundle: &Bundle,
    edits: &EditMap,
    smali_edits: &SmaliEditMap,
    additions: &ApkAdditions,
    out_path: &Path,
) -> Result<()> {
    let empty_plist: PlistEditMap = std::collections::BTreeMap::new();
    export_to_path_full(bundle, edits, smali_edits, additions, &empty_plist, out_path)
}

/// Full export entry point — handles every bundle kind and
/// every edit map. Carry the right set per kind:
///   * Native: `edits` only.
///   * APK: `edits` + `smali_edits` + `additions`.
///   * IPA: `edits` + `plist_edits`.
/// Passing the wrong set for a bundle is caller error.
pub fn export_to_path_full(
    bundle: &Bundle,
    edits: &EditMap,
    smali_edits: &SmaliEditMap,
    additions: &ApkAdditions,
    plist_edits: &PlistEditMap,
    out_path: &Path,
) -> Result<()> {
    if edits.is_empty()
        && smali_edits.is_empty()
        && additions.is_empty()
        && plist_edits.is_empty()
    {
        bail!("no edits to export");
    }
    match bundle.kind {
        BundleKind::Native => {
            if !smali_edits.is_empty()
                || !additions.is_empty()
                || !plist_edits.is_empty()
            {
                bail!("native bundle can't carry smali / APK / plist edits");
            }
            export_native_to_path(bundle, edits, out_path)
        }
        BundleKind::Apk => {
            if !plist_edits.is_empty() {
                bail!("APK bundle can't carry plist edits");
            }
            export_apk_to_path(bundle, edits, smali_edits, additions, out_path)
        }
        BundleKind::Ipa => {
            if !smali_edits.is_empty() || !additions.is_empty() {
                bail!("IPA bundle can't carry smali edits or APK additions");
            }
            export_ipa_to_path(bundle, edits, plist_edits, out_path)
        }
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

fn export_apk_to_path(
    bundle: &Bundle,
    edits: &EditMap,
    smali_edits: &SmaliEditMap,
    additions: &ApkAdditions,
    out: &Path,
) -> Result<()> {
    use smali::android::zip::ApkFile;

    // Collect every changed-or-new component into a single
    // (entry_name → bytes) table. The smali crate's
    // `replace_entry` is `insert` semantically — it adds when
    // the key is missing — so the same loop handles both
    // replacements and additions. We just emit a tracing event
    // when something looks like a fresh add so the export log
    // is legible.
    let mut overrides: Vec<(String, Vec<u8>)> = Vec::new();
    collect_native_overrides(bundle, edits, &mut overrides)?;
    collect_dex_overrides(bundle, smali_edits, &mut overrides)?;

    let mut apk = ApkFile::from_file(&bundle.source_path)
        .map_err(|e| anyhow!("opening APK {}: {e:?}", bundle.source_path.display()))?;
    for (entry_name, new_bytes) in overrides {
        apk.replace_entry(&entry_name, new_bytes)
            .map_err(|e| anyhow!("replacing {entry_name} in APK: {e:?}"))?;
    }
    // Brand-new entries last, so additions appear after any
    // replacements in the deterministic export ordering. The
    // smali crate's ApkFile stores entries in a sorted map, so
    // ordering is actually stable regardless, but doing
    // additions explicitly later keeps the log readable.
    for (entry_name, bytes) in additions {
        apk.replace_entry(entry_name.as_str(), bytes.clone())
            .map_err(|e| anyhow!("adding {entry_name} to APK: {e:?}"))?;
    }
    apk.write_to_file(out)
        .map_err(|e| anyhow!("writing patched APK {}: {e:?}", out.display()))?;
    Ok(())
}

/// Build (entry_name, new_bytes) pairs for every native lib that
/// has staged byte edits. Native libs live at `lib/<abi>/<name>`
/// — the artifact's `label` is exactly that suffix (set at load
/// time).
fn collect_native_overrides(
    bundle: &Bundle,
    edits: &EditMap,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    for (artifact_id, patches) in edits {
        let artifact = bundle
            .artifacts
            .iter()
            .find(|a| &a.id == artifact_id)
            .ok_or_else(|| anyhow!("artifact for edit not found in bundle"))?;
        let entry_name = if artifact.label.starts_with("lib/") {
            artifact.label.clone()
        } else {
            format!("lib/{}", artifact.label)
        };
        let new_bytes = export_native_artifact(&artifact.binary.bytes, patches)
            .with_context(|| format!("re-serialising {entry_name}"))?;
        out.push((entry_name, new_bytes));
    }
    Ok(())
}

/// Build (entry_name, new_bytes) pairs for every DEX that has
/// staged smali edits. Edited classes are spliced into the
/// original class list (load order preserved aside from a final
/// JNI-name sort that mirrors `smali2dex`), then the whole DEX
/// is re-emitted via `DexFile::from_smali`.
fn collect_dex_overrides(
    bundle: &Bundle,
    smali_edits: &SmaliEditMap,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    use smali::dex::DexFile;
    for (dex_aid, class_edits) in smali_edits {
        if class_edits.is_empty() {
            continue;
        }
        let group = bundle
            .dex_groups
            .iter()
            .find(|g| &g.artifact_id == dex_aid)
            .ok_or_else(|| {
                anyhow!("smali edit references unknown DEX artifact {dex_aid}")
            })?;
        let mut classes: Vec<smali::types::SmaliClass> = group
            .classes
            .iter()
            .map(|c| {
                let jni = c.name.as_jni_type();
                class_edits.get(&jni).cloned().unwrap_or_else(|| c.clone())
            })
            .collect();
        classes.sort_by(|a, b| a.name.as_jni_type().cmp(&b.name.as_jni_type()));
        let dex = DexFile::from_smali(&classes)
            .map_err(|e| anyhow!("assembling DEX {}: {e:?}", group.name))?;
        out.push((group.name.clone(), dex.to_bytes().to_vec()));
    }
    Ok(())
}

fn export_ipa_to_path(
    bundle: &Bundle,
    edits: &EditMap,
    plist_edits: &PlistEditMap,
    out: &Path,
) -> Result<()> {
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
    // Plist edits arrive already keyed by archive path and
    // already in their original on-disk format — drop them
    // straight into the override map. If a path collides with
    // a native edit (shouldn't happen — plists aren't native
    // artifacts) the plist edit wins as the last writer.
    for (path, bytes) in plist_edits {
        entry_overrides.insert(path.clone(), bytes.clone());
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
