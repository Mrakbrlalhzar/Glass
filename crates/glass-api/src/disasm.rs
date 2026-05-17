//! Disassembly verbs — linear sweep, per-function, single-word decode.

use anyhow::{Context, Result};
use armv8_encode::container::{Container, SectionKind};
use armv8_encode::isa::aarch64;
use glass_arch_arm64::{format as fmt, SymbolMap};
use serde::Serialize;

use crate::bundle::Bundle;

#[derive(Serialize, Debug, Clone)]
pub struct DisasmListing {
    pub artifact: String,
    pub section: String,
    pub base_address: String,
    pub total_instructions: usize,
    pub shown: usize,
    pub rows: Vec<DisasmRow>,
}

#[derive(Serialize, Debug, Clone)]
pub struct DisasmRow {
    pub address: String,
    pub bytes: String,
    pub mnemonic: String,
    pub operands: String,
    /// Only populated when the row's address starts a known symbol.
    pub symbol: Option<String>,
    /// Resolved branch / ADRP target text — e.g. "foo+0x4" or `None`.
    pub comment: Option<String>,
    pub undecoded: bool,
}

impl Bundle {
    /// Linear sweep over a text section. Picks the first text
    /// section in `artifact_ref` when `section_filter` is None.
    /// `limit` caps the number of rows; subsequent rows are
    /// dropped silently (the `shown` field carries the actual
    /// count and `total_instructions` the section's instruction
    /// count).
    pub fn disasm(
        &self,
        artifact_ref: &str,
        section_filter: Option<&str>,
        limit: Option<usize>,
    ) -> Result<DisasmListing> {
        let art = self
            .artifacts
            .iter()
            .find(|a| {
                a.label == artifact_ref
                    || a.id.to_string().starts_with(artifact_ref)
            })
            .with_context(|| format!("no artifact matches {artifact_ref:?}"))?;
        let container = &art.binary.container;
        let section = pick_text_section(container, section_filter)
            .with_context(|| {
                if let Some(name) = section_filter {
                    format!("section {name:?} not found / not text")
                } else {
                    "no text section in this artifact".to_string()
                }
            })?;
        let total_instructions = section.bytes.len() / 4;
        let rows = sweep_section(
            section,
            &SymbolMap::build(container),
            limit.unwrap_or(usize::MAX),
        );
        let shown = rows.len();
        Ok(DisasmListing {
            artifact: art.id.to_string(),
            section: section.name.clone(),
            base_address: format!("0x{:x}", section.address),
            total_instructions,
            shown,
            rows,
        })
    }
}

fn pick_text_section<'a>(
    container: &'a Container,
    name: Option<&str>,
) -> Option<&'a armv8_encode::container::Section> {
    container
        .sections
        .iter()
        .find(|s| matches!(s.kind, SectionKind::Text) && name.is_none_or(|n| s.name == n))
}

fn sweep_section(
    section: &armv8_encode::container::Section,
    symbols: &SymbolMap,
    cap: usize,
) -> Vec<DisasmRow> {
    let base = section.address;
    let bytes: &[u8] = &section.bytes;
    let n = bytes.len() / 4;
    let mut rows = Vec::with_capacity(n.min(cap));
    for i in 0..n {
        if rows.len() >= cap {
            break;
        }
        let addr = base + (i as u64) * 4;
        let word = u32::from_le_bytes([
            bytes[i * 4],
            bytes[i * 4 + 1],
            bytes[i * 4 + 2],
            bytes[i * 4 + 3],
        ]);
        let symbol = symbols
            .at(addr)
            .map(|s| s.display_name.clone());
        match aarch64::decode_instruction(addr, word) {
            Ok(insn) => {
                let mnemonic = fmt::mnemonic_chunk(&insn).text;
                let operands = fmt::operands_chunks(&insn)
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<Vec<_>>()
                    .join("");
                let comment = fmt::primary_address_operand(&insn).and_then(|t| {
                    let sym = symbols.covering(t)?;
                    let off = t - sym.address;
                    Some(if off == 0 {
                        sym.display_name.clone()
                    } else {
                        format!("{}+0x{off:x}", sym.display_name)
                    })
                });
                rows.push(DisasmRow {
                    address: format!("0x{:016x}", addr),
                    bytes: format!(
                        "{:02x} {:02x} {:02x} {:02x}",
                        bytes[i * 4],
                        bytes[i * 4 + 1],
                        bytes[i * 4 + 2],
                        bytes[i * 4 + 3],
                    ),
                    mnemonic,
                    operands,
                    symbol,
                    comment,
                    undecoded: false,
                });
            }
            Err(_) => {
                rows.push(DisasmRow {
                    address: format!("0x{:016x}", addr),
                    bytes: format!(
                        "{:02x} {:02x} {:02x} {:02x}",
                        bytes[i * 4],
                        bytes[i * 4 + 1],
                        bytes[i * 4 + 2],
                        bytes[i * 4 + 3],
                    ),
                    mnemonic: ".word".to_string(),
                    operands: format!("0x{word:08x}"),
                    symbol,
                    comment: None,
                    undecoded: true,
                });
            }
        }
    }
    rows
}

// ---- single-word decode ----------------------------------------------------

#[derive(Serialize, Debug, Clone)]
pub struct DecodeResult {
    pub word: String,
    pub mnemonic: String,
    pub operands: String,
    pub undecoded: bool,
}

/// Decode one 32-bit AArch64 instruction word. `addr` is the
/// instruction's address — affects PC-relative branch decoding.
pub fn decode_word(word: u32, addr: u64) -> DecodeResult {
    match aarch64::decode_instruction(addr, word) {
        Ok(insn) => DecodeResult {
            word: format!("0x{word:08x}"),
            mnemonic: fmt::mnemonic_chunk(&insn).text,
            operands: fmt::operands_chunks(&insn)
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join(""),
            undecoded: false,
        },
        Err(_) => DecodeResult {
            word: format!("0x{word:08x}"),
            mnemonic: ".word".to_string(),
            operands: format!("0x{word:08x}"),
            undecoded: true,
        },
    }
}
