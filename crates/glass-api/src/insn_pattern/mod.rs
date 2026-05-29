//! Typed-assembly instruction patterns.
//!
//! Per-call grammar:
//!
//!   - Parses one or more `;`-separated assembly instructions.
//!   - Operands: GP registers, immediates (decimal or hex,
//!     optional `#`), simple memory forms, register lists
//!     (ARMv7), AND wildcards (`<*>`, `<W>`, `<X>`, `<imm>`,
//!     bare `r`, `<R>`, …).
//!   - Drives the upstream encoder after substituting
//!     placeholder values for wildcards; clears the bits a
//!     wildcard owns in the per-byte mask so the bin-search
//!     engine accepts any value in those positions.
//!   - Output: `Vec<Atom>` of `(mask, value)` byte atoms.
//!
//! ## Module layout
//!
//! - [`shared`] — ISA-agnostic helpers (immediate parsing, the
//!   bracket-aware comma splitter, the symbol heuristic,
//!   [`shared::CompileOptions`], [`shared::SymbolLookup`]).
//! - [`aarch64`] — AArch64 compile/parse internals.
//! - [`armv7`]   — Thumb/T32 + ARM/A32 compile/parse internals.
//!
//! See `docs/InsnPattern.md` for the full design.

use anyhow::{Context, Result};
use serde::Serialize;

use crate::bin_search::{Atom, BinMatch, BinSearchResult};
use crate::bundle::Bundle;

pub mod aarch64;
pub mod armv7;
pub mod shared;

#[cfg(test)]
mod armv7_tests;

#[derive(Serialize, Debug, Clone)]
pub struct InsnSearchResult {
    pub artifact: String,
    pub pattern: String,
    /// Hex bytes the pattern compiled to — useful for
    /// debugging and for piping into a follow-up `bin-search`.
    pub bytes_hex: String,
    pub total: usize,
    pub shown: usize,
    pub matches: Vec<BinMatch>,
}

// ---- Re-exported public AArch64 surface ----------------------
//
// These names match the pre-split paths so external callers
// (CLI, GUI, scripting) keep working unchanged.

pub use aarch64::{compile, compile_at, compile_to_atoms};

// ---- Bundle method --------------------------------------------

impl Bundle {
    /// Compile `pattern` (one or more `;`-separated assembly
    /// instructions) to byte atoms and scan the artifact for
    /// them. Supports concrete operands and wildcards. Routes
    /// AArch64 artifacts through the AArch64 encoder and ARM
    /// (ARMv7) artifacts through the Thumb/A32 encoders; other
    /// architectures fail with a clear error.
    pub fn insn_search(
        &self,
        artifact_ref: &str,
        pattern: &str,
        section_filter: Option<&str>,
        limit: Option<usize>,
    ) -> Result<InsnSearchResult> {
        let art = self
            .artifacts
            .iter()
            .find(|a| {
                a.label == artifact_ref || a.id.to_string().starts_with(artifact_ref)
            })
            .with_context(|| format!("no artifact matches {artifact_ref:?}"))?;
        let arch = art.binary.container.architecture;
        let atoms = compile_insn_atoms_for_arch(pattern, arch)
            .with_context(|| format!("compiling pattern {pattern:?}"))?;
        if atoms.is_empty() {
            anyhow::bail!("pattern compiled to zero atoms");
        }
        let bytes_hex = shared::atoms_to_hex(&atoms);
        // Reuse the bin-search backend so navigation, previews,
        // and section filtering all behave identically.
        let bin = self.bin_search_with_atoms(
            artifact_ref,
            &bytes_hex,
            &atoms,
            section_filter,
            limit,
        )?;
        Ok(InsnSearchResult {
            artifact: bin.artifact,
            pattern: pattern.to_string(),
            bytes_hex,
            total: bin.total,
            shown: bin.shown,
            matches: bin.matches,
        })
    }

    /// Shared backend used by `insn_search`. Same logic as
    /// `bin_search` but takes pre-compiled atoms instead of a
    /// pattern string.
    fn bin_search_with_atoms(
        &self,
        artifact_ref: &str,
        pattern_text: &str,
        atoms: &[Atom],
        section_filter: Option<&str>,
        limit: Option<usize>,
    ) -> Result<BinSearchResult> {
        let art = self
            .artifacts
            .iter()
            .find(|a| {
                a.label == artifact_ref || a.id.to_string().starts_with(artifact_ref)
            })
            .with_context(|| format!("no artifact matches {artifact_ref:?}"))?;
        let container = &art.binary.container;
        let arch = container.architecture;
        let mut matches: Vec<BinMatch> = Vec::new();
        let mut total = 0usize;
        let cap = limit.unwrap_or(usize::MAX);
        for section in &container.sections {
            if let Some(name) = section_filter {
                if section.name != name {
                    continue;
                }
            }
            use armv8_encode::container::SectionKind;
            match section.kind {
                SectionKind::Bss | SectionKind::Debug => continue,
                _ => {}
            }
            if section.address == 0 || section.bytes.is_empty() {
                continue;
            }
            let is_text = matches!(section.kind, SectionKind::Text);
            for (start, slice_end) in crate::bin_search::scan_section(atoms, &section.bytes) {
                let abs_end = start + slice_end;
                total += 1;
                if matches.len() >= cap {
                    continue;
                }
                let preview = crate::bin_search::build_preview(
                    is_text,
                    arch,
                    section.address + start as u64,
                    &section.bytes[start..abs_end.min(section.bytes.len())],
                );
                matches.push(BinMatch {
                    section: section.name.clone(),
                    address: format!("0x{:x}", section.address + start as u64),
                    length: slice_end,
                    preview,
                });
            }
        }
        Ok(BinSearchResult {
            artifact: art.id.to_string(),
            pattern: pattern_text.to_string(),
            total,
            shown: matches.len(),
            matches,
        })
    }
}

// ---- Architecture-aware compile dispatchers -------------------

/// Compile `pattern` to the byte atoms appropriate for the
/// supplied architecture. AArch64 goes through the AArch64
/// compiler; ARM (ARMv7) routes to the Thumb/A32 compiler.
/// Other architectures error out.
pub fn compile_insn_atoms_for_arch(
    pattern: &str,
    arch: armv8_encode::container::Architecture,
) -> Result<Vec<Atom>> {
    use armv8_encode::container::Architecture;
    match arch {
        Architecture::Aarch64 => compile_to_atoms(pattern),
        Architecture::Arm => armv7::compile_armv7_to_atoms(pattern),
        other => anyhow::bail!(
            "insn-search: typed-assembly grammar isn't implemented for {other:?}; \
             use bin-search with a hex pattern"
        ),
    }
}

/// Compile `pattern` for every architecture Glass's typed-
/// assembly grammar supports, returning a `(arch, atoms)` entry
/// per architecture whose parse + encode succeeded. Used by the
/// global-scan palette path so an AArch64 pattern lands on
/// AArch64 artifacts and an ARMv7 pattern lands on ARMv7
/// artifacts, without the caller having to decide up-front.
///
/// Returns an error only if *no* architecture accepted the
/// pattern. Per-arch errors are swallowed since "ARMv7 syntax
/// doesn't parse as AArch64" is the expected case, not a
/// failure mode.
pub fn compile_insn_atoms_for_all_arches(
    pattern: &str,
) -> Result<Vec<(armv8_encode::container::Architecture, Vec<Atom>)>> {
    use armv8_encode::container::Architecture;
    let mut out = Vec::new();
    let mut last_err: Option<String> = None;
    match compile_to_atoms(pattern) {
        Ok(a) => out.push((Architecture::Aarch64, a)),
        Err(e) => last_err = Some(format!("aarch64: {e:#}")),
    }
    match armv7::compile_armv7_to_atoms(pattern) {
        Ok(a) => out.push((Architecture::Arm, a)),
        Err(e) => {
            let msg = format!("armv7: {e:#}");
            last_err = Some(match last_err {
                Some(prev) => format!("{prev}; {msg}"),
                None => msg,
            });
        }
    }
    if out.is_empty() {
        anyhow::bail!(
            "pattern doesn't parse as any supported architecture ({})",
            last_err.unwrap_or_else(|| "no diagnostic".to_string())
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    /// End-to-end: compile an ARMv7 pattern through the typed-
    /// assembly grammar and scan the libtool-checker.so fixture
    /// for matches. Sanity-checks that the architecture dispatch
    /// routes ARM artifacts to the Thumb encoder and that a
    /// well-formed pattern returns >0 matches against a known-
    /// non-empty Thumb binary.
    #[test]
    fn insn_search_finds_matches_in_armv7_fixture() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("glass-arch-arm")
            .join("tests")
            .join("libtool-checker.so");
        if !fixture.exists() {
            eprintln!("skipping: fixture not found at {}", fixture.display());
            return;
        }
        let bundle = crate::bundle::open(&fixture).expect("open fixture bundle");
        let label = bundle.artifacts[0].label.clone();
        let res = bundle
            .insn_search(&label, "bx lr", None, Some(64))
            .expect("insn_search");
        assert!(res.total > 0, "expected matches for 'bx lr', got {}", res.total);
    }
}
