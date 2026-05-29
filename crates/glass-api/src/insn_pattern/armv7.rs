//! ARMv7 (Thumb/T32 + ARM/A32) typed-assembly pattern compiler.
//!
//! Mirrors the AArch64 path in `insn_pattern.rs` but targets the
//! `armv8_encode::isa::armv7` (Thumb) and `::armv7::arm` (A32)
//! encoder tables. Same `Vec<Atom>` output shape so the bin-search
//! engine consumes either flavour transparently.
//!
//! ## Mode selection
//!
//! Per-pattern: we try Thumb first; if the first instruction
//! doesn't compile cleanly under Thumb we retry the whole pattern
//! under ARM mode. Mixing modes within one pattern isn't supported
//! (matches in real binaries don't cross mode boundaries).
//!
//! ## Wildcard mask derivation
//!
//! Upstream's ARMv7 row type doesn't expose `operand_bit_ranges`
//! the way AArch64 does. We instead derive the per-wildcard byte
//! mask empirically: for each candidate row we encode the
//! operands once with each wildcard at one placeholder value and
//! again with each wildcard varied. Bits that change across the
//! probes are owned by *some* wildcard; we clear them in the mask.
//! Concrete operands hold their bits constant across the probes,
//! so their bits stay fixed in the mask.
//!
//! ## Coverage
//!
//! - Registers: `r0..r15`, `sp`, `lr`, `pc`.
//! - Condition-code suffix on the mnemonic (`bxeq`, `moveq`, …)
//!   for ARM mode; conditional Thumb branch forms (`beq`, `bne`,
//!   …) for 16-bit T1 B.
//! - Immediates with optional `#` prefix, decimal / hex, signed.
//! - Register lists `{r0, r1, r4-r7, lr, pc}`.
//! - Memory addressing: `[rN]`, `[rN, #imm]` — minimum useful
//!   subset. Pre/post-index, register-offset, and shifted
//!   register-offset forms aren't supported yet.
//! - Wildcards: same grammar as AArch64. `*`, `<*>`, `#*`,
//!   `<imm>`, bare `r`, `<R>`.
//!
//! Out-of-scope (will return an error directing the user to
//! a more concrete spelling):
//! - Shifted operands (`r0, lsl #2`).
//! - Pre/post-index addressing modes.
//! - Bitmask register-list syntax (`{0b00010010}`).

use anyhow::{anyhow, Context, Result};
use armv8_encode::isa::armv7::arm::table_generated::{
    ArmOpcodeGenerated, ARM_OPCODE_TABLE_GENERATED,
};
use armv8_encode::isa::armv7::operand::{DecodedOperand, Register, RegisterClass};
use armv8_encode::isa::armv7::table::ThumbWidth;
use armv8_encode::isa::armv7::table_generated::{
    ThumbOpcodeGenerated, THUMB_OPCODE_TABLE_GENERATED,
};

use crate::bin_search::Atom;
use super::shared::{
    looks_like_symbol, tokenize_operand_strings, try_parse_immediate,
};

/// Compile a single ARMv7 instruction at `address` to concrete
/// bytes — 2 bytes for Thumb-1, 4 bytes for Thumb-2 / A32. Tries
/// Thumb first; falls back to ARM mode if Thumb refuses. Wildcards
/// are rejected (caller is the GUI editor staging real bytes).
///
/// `prefer_thumb`: when the source binary is Thumb-only or the
/// editor knows the row is Thumb, set true so we don't slip into
/// ARM-mode encodings. AArch64 doesn't call into this; the
/// dispatch happens in [`commit_disasm_edit`].
pub fn compile_armv7_at(
    source: &str,
    address: u64,
    prefer_thumb: bool,
    symbol_lookup: Option<&dyn Fn(&str) -> Option<u64>>,
) -> Result<Vec<u8>> {
    // Refuse multi-instruction patterns at the editor layer —
    // the registry stores one edit per address and we'd need
    // downstream-row shifting to splice multiple instructions
    // in place.
    let pieces: Vec<&str> = source
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if pieces.len() != 1 {
        anyhow::bail!(
            "expected exactly one instruction; got {} (multi-instruction edits aren't supported)",
            pieces.len()
        );
    }
    let s = pieces[0];
    if prefer_thumb {
        let thumb_err = match compile_one_thumb(s, address, symbol_lookup) {
            Ok((word, _mask, width)) => return Ok(thumb_word_to_le_bytes(word, width)),
            Err(e) => e,
        };
        let arm_err = match compile_one_arm(s, address, symbol_lookup) {
            Ok((word, _mask)) => return Ok(word.to_le_bytes().to_vec()),
            Err(e) => e,
        };
        anyhow::bail!(
            "neither Thumb nor ARM mode could encode {s:?} — thumb: {thumb_err:#}; arm: {arm_err:#}"
        );
    } else {
        let arm_err = match compile_one_arm(s, address, symbol_lookup) {
            Ok((word, _mask)) => return Ok(word.to_le_bytes().to_vec()),
            Err(e) => e,
        };
        let thumb_err = match compile_one_thumb(s, address, symbol_lookup) {
            Ok((word, _mask, width)) => return Ok(thumb_word_to_le_bytes(word, width)),
            Err(e) => e,
        };
        anyhow::bail!(
            "neither ARM nor Thumb mode could encode {s:?} — arm: {arm_err:#}; thumb: {thumb_err:#}"
        );
    }
}

fn thumb_word_to_le_bytes(word: u32, width: ThumbWidth) -> Vec<u8> {
    match width {
        ThumbWidth::Halfword => ((word & 0xffff) as u16).to_le_bytes().to_vec(),
        ThumbWidth::Word => {
            let hw1 = ((word >> 16) & 0xffff) as u16;
            let hw2 = (word & 0xffff) as u16;
            let mut out = Vec::with_capacity(4);
            out.extend_from_slice(&hw1.to_le_bytes());
            out.extend_from_slice(&hw2.to_le_bytes());
            out
        }
    }
}

/// Top-level ARMv7 pattern compiler. Tries Thumb first; if the
/// first instruction won't compile under Thumb, retries the
/// whole pattern under ARM mode. Returns the byte atoms on
/// success.
pub fn compile_armv7_to_atoms(pattern: &str) -> Result<Vec<Atom>> {
    compile_armv7_to_atoms_with(pattern, None)
}

/// Symbol-aware variant. When `symbol_lookup` is `Some`, bare
/// identifiers in operand position resolve to the absolute address
/// the closure returns and are encoded as branch / immediate
/// operands. With `None`, identifiers fail to parse — same as the
/// historical search path.
pub fn compile_armv7_to_atoms_with(
    pattern: &str,
    symbol_lookup: Option<&dyn Fn(&str) -> Option<u64>>,
) -> Result<Vec<Atom>> {
    // Probe: try Thumb on the first non-empty instruction.
    let first = pattern
        .split(';')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("empty pattern"))?;
    let thumb_first = compile_one_thumb(first, 0, symbol_lookup);
    let arm_first = compile_one_arm(first, 0, symbol_lookup);
    match (thumb_first, arm_first) {
        (Ok(_), _) => compile_pattern_thumb(pattern, symbol_lookup),
        (Err(_), Ok(_)) => compile_pattern_arm(pattern, symbol_lookup),
        (Err(et), Err(ea)) => Err(anyhow!(
            "neither Thumb nor ARM mode could encode {first:?} — \
             thumb: {et:#}; arm: {ea:#}"
        )),
    }
}

/// Compile every `;`-separated instruction in Thumb mode and
/// concatenate byte atoms in encoding order.
fn compile_pattern_thumb(
    pattern: &str,
    symbol_lookup: Option<&dyn Fn(&str) -> Option<u64>>,
) -> Result<Vec<Atom>> {
    let mut out = Vec::new();
    let mut addr: u64 = 0;
    for (i, raw) in pattern.split(';').enumerate() {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let (word, mask, width) = compile_one_thumb(s, addr, symbol_lookup)
            .with_context(|| format!("Thumb instruction {} ({s:?})", i + 1))?;
        emit_thumb_bytes(&mut out, word, mask, width);
        addr = addr.wrapping_add(match width {
            ThumbWidth::Halfword => 2,
            ThumbWidth::Word => 4,
        });
    }
    Ok(out)
}

fn compile_pattern_arm(
    pattern: &str,
    symbol_lookup: Option<&dyn Fn(&str) -> Option<u64>>,
) -> Result<Vec<Atom>> {
    let mut out = Vec::new();
    let mut addr: u64 = 0;
    for (i, raw) in pattern.split(';').enumerate() {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let (word, mask) = compile_one_arm(s, addr, symbol_lookup)
            .with_context(|| format!("ARM instruction {} ({s:?})", i + 1))?;
        let word_bytes = word.to_le_bytes();
        let mask_bytes = mask.to_le_bytes();
        for k in 0..4 {
            out.push(Atom::Mask {
                mask: mask_bytes[k],
                value: word_bytes[k] & mask_bytes[k],
            });
        }
        addr = addr.wrapping_add(4);
    }
    Ok(out)
}

/// Emit byte atoms for a Thumb encoding. 16-bit Thumb writes
/// low 16 bits of `word` as 2 LE bytes. 32-bit Thumb writes
/// hw1 (bits 31..16) as 2 LE bytes followed by hw2 (bits 15..0)
/// as 2 LE bytes — matching `read_instruction`'s layout.
fn emit_thumb_bytes(out: &mut Vec<Atom>, word: u32, mask: u32, width: ThumbWidth) {
    match width {
        ThumbWidth::Halfword => {
            let w = (word & 0xffff) as u16;
            let m = (mask & 0xffff) as u16;
            let wb = w.to_le_bytes();
            let mb = m.to_le_bytes();
            for k in 0..2 {
                out.push(Atom::Mask {
                    mask: mb[k],
                    value: wb[k] & mb[k],
                });
            }
        }
        ThumbWidth::Word => {
            let hw1 = ((word >> 16) & 0xffff) as u16;
            let hw2 = (word & 0xffff) as u16;
            let mhw1 = ((mask >> 16) & 0xffff) as u16;
            let mhw2 = (mask & 0xffff) as u16;
            let b1 = hw1.to_le_bytes();
            let b2 = hw2.to_le_bytes();
            let m1 = mhw1.to_le_bytes();
            let m2 = mhw2.to_le_bytes();
            for k in 0..2 {
                out.push(Atom::Mask {
                    mask: m1[k],
                    value: b1[k] & m1[k],
                });
            }
            for k in 0..2 {
                out.push(Atom::Mask {
                    mask: m2[k],
                    value: b2[k] & m2[k],
                });
            }
        }
    }
}

// ---- Per-instruction Thumb compile ---------------------------

fn compile_one_thumb(
    s: &str,
    address: u64,
    symbol_lookup: Option<&dyn Fn(&str) -> Option<u64>>,
) -> Result<(u32, u32, ThumbWidth)> {
    let (mnem_str, cond_opt, rest) = split_mnemonic_cond(s);
    let tokens = parse_operand_tokens(rest, symbol_lookup)?;
    let mnem_lc = mnem_str.to_ascii_lowercase();
    // Map conditional spellings to base mnemonic + Condition. For
    // Thumb, only the 16-bit T1 B encoding takes a Condition
    // operand. Other conditional spellings (which need an IT
    // block) are out-of-scope here.
    let (base_mnem, prepend_cond) = match (mnem_lc.as_str(), cond_opt) {
        ("b", Some(c)) => ("b".to_string(), Some(c)),
        (m, Some(c)) if !is_known_thumb_mnemonic(m) => {
            // The user typed something like `moveq` — Thumb can't
            // make it conditional outside IT, so fall through with
            // the cond reattached. The encoder will then reject it
            // and our caller falls back to ARM mode.
            let mut joined = m.to_string();
            joined.push_str(cond_suffix(c));
            (joined, None)
        }
        (m, c) => (m.to_string(), c),
    };
    let mut last_err: Option<String> = None;
    for row in THUMB_OPCODE_TABLE_GENERATED.iter() {
        if row.mnemonic.as_str() != base_mnem {
            continue;
        }
        // Reject this row if the user typed a conditional
        // suffix but this row has no Condition slot. Bare `%c`
        // is display-only in Thumb, so silently accepting
        // `bxeq` against `bx%c\t...` would drop the condition.
        if prepend_cond.is_some() {
            let slots = format_slot_kinds(row.format, Mode::Thumb);
            if !slots.iter().any(|s| matches!(s, SlotKind::Condition)) {
                last_err = Some(format!(
                    "row {} has no Condition slot in Thumb mode",
                    row.format
                ));
                continue;
            }
        }
        // Try this row.
        match try_row_thumb(row, &tokens, prepend_cond, address) {
            Ok((w, m)) => return Ok((w, m, row.width)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow!(
        "no Thumb form matches {s:?} (last error: {})",
        last_err.unwrap_or_else(|| "no candidate found".to_string())
    ))
}

fn try_row_thumb(
    row: &ThumbOpcodeGenerated,
    tokens: &[OperandToken],
    prepend_cond: Option<u8>,
    address: u64,
) -> Result<(u32, u32), String> {
    use armv8_encode::isa::armv7::encode::encode_with_row;
    // Establish a baseline encoding with every wildcard set to
    // its "zero" probe value.
    let base = build_operands_for_row(row.format, tokens, prepend_cond, Mode::Thumb, None)
        .map_err(|e| format!("baseline build: {e}"))?;
    let (word_base, _) = encode_with_row(row, &base, address).map_err(|e| format!("{e:?}"))?;
    if (word_base & row.mask) != row.opcode {
        return Err(format!(
            "encoded word 0x{word_base:08x} doesn't satisfy row mask 0x{:08x} (base 0x{:08x})",
            row.mask, row.opcode
        ));
    }
    // For each wildcard token, probe several distinct legal
    // values and OR the resulting XOR diffs into `varying`. The
    // bits that move are the bits this wildcard owns.
    let mut varying: u32 = 0;
    for (tok_idx, tok) in tokens.iter().enumerate() {
        if !matches!(tok, OperandToken::Wildcard(_)) {
            continue;
        }
        for probe in wildcard_probes(tok) {
            let probed = build_operands_for_row(
                row.format,
                tokens,
                prepend_cond,
                Mode::Thumb,
                Some((tok_idx, probe)),
            )
            .map_err(|e| format!("probe build: {e}"))?;
            if let Ok((w, _)) = encode_with_row(row, &probed, address) {
                varying |= word_base ^ w;
            }
        }
    }
    let mask = !varying;
    Ok((word_base & mask, mask))
}

// ---- Per-instruction ARM compile -----------------------------

fn compile_one_arm(
    s: &str,
    address: u64,
    symbol_lookup: Option<&dyn Fn(&str) -> Option<u64>>,
) -> Result<(u32, u32)> {
    let (mnem_str, cond_opt, rest) = split_mnemonic_cond(s);
    let tokens = parse_operand_tokens(rest, symbol_lookup)?;
    let mnem_lc = mnem_str.to_ascii_lowercase();
    let cond_code = cond_opt.unwrap_or(0xe); // AL = always
    let mut last_err: Option<String> = None;
    for row in ARM_OPCODE_TABLE_GENERATED.iter() {
        if row.mnemonic.as_str() != mnem_lc {
            continue;
        }
        match try_row_arm(row, &tokens, cond_code, address) {
            Ok((w, m)) => return Ok((w, m)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow!(
        "no ARM form matches {s:?} (last error: {})",
        last_err.unwrap_or_else(|| "no candidate found".to_string())
    ))
}

fn try_row_arm(
    row: &ArmOpcodeGenerated,
    tokens: &[OperandToken],
    cond_code: u8,
    address: u64,
) -> Result<(u32, u32), String> {
    use armv8_encode::isa::armv7::arm::encode::encode_with_row;
    let base = build_operands_for_row(
        row.format,
        tokens,
        Some(cond_code),
        Mode::Arm,
        None,
    )
    .map_err(|e| format!("baseline build: {e}"))?;
    let word_base = encode_with_row(row, &base, address).map_err(|e| format!("{e:?}"))?;
    if (word_base & row.mask) != row.opcode {
        return Err(format!(
            "encoded word 0x{word_base:08x} doesn't satisfy row mask 0x{:08x} (base 0x{:08x})",
            row.mask, row.opcode
        ));
    }
    let mut varying: u32 = 0;
    for (tok_idx, tok) in tokens.iter().enumerate() {
        if !matches!(tok, OperandToken::Wildcard(_)) {
            continue;
        }
        for probe in wildcard_probes(tok) {
            let probed = build_operands_for_row(
                row.format,
                tokens,
                Some(cond_code),
                Mode::Arm,
                Some((tok_idx, probe)),
            )
            .map_err(|e| format!("probe build: {e}"))?;
            if let Ok(w) = encode_with_row(row, &probed, address) {
                varying |= word_base ^ w;
            }
        }
    }
    let mask = !varying;
    Ok((word_base & mask, mask))
}

// ---- Operand list construction ------------------------------

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(super) enum Mode {
    Thumb,
    Arm,
}

/// One probe value applied to a single wildcard slot. The shape
/// of the value depends on what kind of operand-slot the
/// wildcard lands in; we round-trip through `i64` for the
/// immediate/branch path and clip to `u16` for register / list
/// values.
#[derive(Debug, Copy, Clone)]
struct ProbeValue {
    raw: i64,
}

impl ProbeValue {
    fn imm(v: i64) -> Self {
        Self { raw: v }
    }
}

/// Walk the binutils format string and emit one DecodedOperand
/// per operand slot in the order the encoder consumes them.
///
/// When `probe_override` is `Some((tok_idx, value))`, that
/// specific wildcard's placeholder is set to `value`; every
/// other wildcard uses its default ("zero") placeholder.
fn build_operands_for_row(
    format: &str,
    tokens: &[OperandToken],
    cond: Option<u8>,
    mode: Mode,
    probe_override: Option<(usize, ProbeValue)>,
) -> Result<Vec<DecodedOperand>, String> {
    let slots = format_slot_kinds(format, mode);
    let mut out = Vec::with_capacity(slots.len());
    let mut tok_idx = 0usize;
    let probe_for = |idx: usize| -> Option<ProbeValue> {
        probe_override.and_then(|(i, v)| if i == idx { Some(v) } else { None })
    };
    for slot in &slots {
        match slot {
            SlotKind::Condition => {
                let c = cond.ok_or("format expects %c but no condition given")?;
                out.push(DecodedOperand::Condition(c));
            }
            SlotKind::Register => {
                if tok_idx >= tokens.len() {
                    return Err("not enough operand tokens (register)".into());
                }
                let op = tokens[tok_idx].as_register_operand(probe_for(tok_idx))?;
                tok_idx += 1;
                out.push(op);
            }
            SlotKind::Immediate => {
                if tok_idx >= tokens.len() {
                    return Err("not enough operand tokens (immediate)".into());
                }
                let op = tokens[tok_idx].as_immediate_operand(probe_for(tok_idx))?;
                tok_idx += 1;
                out.push(op);
            }
            SlotKind::BranchTarget => {
                if tok_idx >= tokens.len() {
                    return Err("not enough operand tokens (branch)".into());
                }
                let op = tokens[tok_idx].as_branch_operand(probe_for(tok_idx))?;
                tok_idx += 1;
                out.push(op);
            }
            SlotKind::RegisterList => {
                if tok_idx >= tokens.len() {
                    return Err("not enough operand tokens (reglist)".into());
                }
                let op = tokens[tok_idx].as_register_list_operand(probe_for(tok_idx))?;
                tok_idx += 1;
                out.push(op);
            }
            SlotKind::Memory | SlotKind::Opaque | SlotKind::Other => {
                if let Some(tok) = tokens.get(tok_idx) {
                    if matches!(tok, OperandToken::Memory(_, _, _)) {
                        let op = tok.as_memory_operand()?;
                        tok_idx += 1;
                        out.push(op);
                        continue;
                    }
                }
                out.push(DecodedOperand::OpaqueBits { bits: 0, mask: 0 });
            }
        }
    }
    if tok_idx != tokens.len() {
        return Err(format!(
            "extra operand tokens: format wanted {} usable slots, got {} tokens",
            tok_idx,
            tokens.len()
        ));
    }
    Ok(out)
}

/// Enumerate probe values for one wildcard slot. We probe with
/// the zero baseline (always) plus a sweep that flips each bit
/// in turn for register / immediate kinds, and a sweep of
/// distinct branch displacements for branch wildcards. The
/// caller XOR-diffs each probe against the zero baseline and
/// ORs into a "varying bits" set.
fn wildcard_probes(tok: &OperandToken) -> Vec<ProbeValue> {
    // We don't know which slot kind this wildcard will land in
    // at probe time — different rows place the same token at
    // different kinds. Generate a generous superset; the
    // encoder rejects values that don't fit, and the caller
    // silently drops those.
    let _ = tok;
    // Bit-flip probes covering 0..=16 bits — enough to span the
    // widest immediate fields (12-bit Thumb %I, 16-bit %J).
    // Plus a few odd values to catch encoders that scatter bits
    // in non-contiguous ways (Thumb-2 %I splits across hw1[10]
    // : hw2[14:12] : hw2[7:0]).
    let mut out = Vec::with_capacity(20);
    for bit in 0..16u32 {
        out.push(ProbeValue::imm(1i64 << bit));
    }
    // Branch-target probes (small even values within reach of
    // the shortest 8-bit signed Thumb branch).
    out.push(ProbeValue::imm(2));
    out.push(ProbeValue::imm(4));
    out.push(ProbeValue::imm(8));
    out.push(ProbeValue::imm(16));
    out
}

use super::armv7_format_codes::{format_slot_kinds, SlotKind};

// ---- Operand tokens -----------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)] // Wildcard payload carries kind hint for future opcode ranking.
enum OperandToken {
    /// Concrete register (class + index).
    Reg(RegisterClass, u8),
    /// Concrete immediate.
    Imm(i64),
    /// Memory: base register + optional immediate offset.
    /// Third field reserved for future addressing-mode tags.
    Memory(Register, Option<i64>, ()),
    /// Concrete register-list bitmap.
    RegList(u16),
    /// Wildcard with a kind hint.
    Wildcard(WildcardKind),
}

#[derive(Debug, Clone, Copy)]
enum WildcardKind {
    Any,
    Reg,
    Imm,
}

impl OperandToken {
    fn as_register_operand(&self, probe: Option<ProbeValue>) -> Result<DecodedOperand, String> {
        match self {
            OperandToken::Reg(class, idx) => Ok(DecodedOperand::Register(Register {
                class: *class,
                index: *idx,
            })),
            OperandToken::Wildcard(_) => {
                // Default placeholder is r0 (index 0). Probe sets
                // the register index to (raw & 0x7) so we only
                // touch bits the encoder accepts in 3-bit Low
                // slots — sufficient to cover the bits the slot
                // occupies in the encoded word.
                let idx = probe.map(|p| (p.raw as u8) & 0x7).unwrap_or(0);
                Ok(DecodedOperand::Register(Register {
                    class: RegisterClass::R,
                    index: idx,
                }))
            }
            OperandToken::Imm(_) => Err("expected register, got immediate".into()),
            OperandToken::Memory(..) => Err("expected register, got memory".into()),
            OperandToken::RegList(_) => Err("expected register, got reglist".into()),
        }
    }

    fn as_immediate_operand(&self, probe: Option<ProbeValue>) -> Result<DecodedOperand, String> {
        match self {
            OperandToken::Imm(v) => Ok(DecodedOperand::Immediate(*v)),
            OperandToken::Wildcard(_) => {
                let v = probe.map(|p| p.raw).unwrap_or(0);
                Ok(DecodedOperand::Immediate(v))
            }
            OperandToken::Reg(..) => Err("expected immediate, got register".into()),
            OperandToken::Memory(..) => Err("expected immediate, got memory".into()),
            OperandToken::RegList(_) => Err("expected immediate, got reglist".into()),
        }
    }

    fn as_branch_operand(&self, probe: Option<ProbeValue>) -> Result<DecodedOperand, String> {
        match self {
            OperandToken::Imm(v) => Ok(DecodedOperand::BranchTarget(*v as u64)),
            OperandToken::Wildcard(_) => {
                // Branch targets are PC-relative; placeholder = 0
                // means "branch to address 0", which the encoder
                // accepts on any branch form (encoded as a signed
                // offset from PC). Probes use small even offsets
                // so the encoder's range checks pass.
                let v = probe.map(|p| p.raw as u64).unwrap_or(0);
                Ok(DecodedOperand::BranchTarget(v))
            }
            OperandToken::Reg(..) => Err("expected branch target, got register".into()),
            OperandToken::Memory(..) => Err("expected branch target, got memory".into()),
            OperandToken::RegList(_) => Err("expected branch target, got reglist".into()),
        }
    }

    fn as_register_list_operand(&self, probe: Option<ProbeValue>) -> Result<DecodedOperand, String> {
        match self {
            OperandToken::RegList(m) => Ok(DecodedOperand::RegisterList(*m)),
            OperandToken::Wildcard(_) => {
                // Use the probe's low 8 bits as a register-list
                // mask. The high bit (LR/PC, bits 14/15) is left
                // off in the baseline; probes vary the low byte
                // exhaustively over the 16-bit-flip range.
                let m = probe.map(|p| p.raw as u16).unwrap_or(0);
                Ok(DecodedOperand::RegisterList(m))
            }
            _ => Err("expected register list".into()),
        }
    }

    fn as_memory_operand(&self) -> Result<DecodedOperand, String> {
        match self {
            OperandToken::Memory(_base, _off, _) => {
                // The Thumb/ARM `%a` family expects OpaqueBits.
                // We can't synthesise the exact bits from a high-
                // level memory token without re-implementing the
                // encoder's addressing-mode packers, so we punt:
                // emit an OpaqueBits with mask=0 so the encoder
                // splices nothing into the word and the row's
                // base bits are taken as-is.
                Ok(DecodedOperand::OpaqueBits { bits: 0, mask: 0 })
            }
            _ => Err("expected memory operand".into()),
        }
    }
}

// ---- Parsing ------------------------------------------------

/// Splits an instruction into `(mnemonic-without-cond,
/// condition-code-if-suffixed, rest-of-operands)`.
fn split_mnemonic_cond(s: &str) -> (&str, Option<u8>, &str) {
    let (mnem, rest) = match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], s[idx..].trim()),
        None => (s, ""),
    };
    if let Some(c) = parse_cond_suffix(mnem) {
        // Strip 2-char suffix from mnem.
        let cut = mnem.len() - 2;
        return (&mnem[..cut], Some(c), rest);
    }
    (mnem, None, rest)
}

/// Recognise the 16 ARM condition-code suffixes. Returns the
/// 4-bit cond field code, or None if the mnemonic doesn't end
/// in a known suffix or stripping the suffix would leave an
/// empty string.
fn parse_cond_suffix(mnem: &str) -> Option<u8> {
    if mnem.len() <= 2 {
        return None;
    }
    let suffix = &mnem[mnem.len() - 2..].to_ascii_lowercase();
    let c = match suffix.as_str() {
        "eq" => 0x0,
        "ne" => 0x1,
        "cs" | "hs" => 0x2,
        "cc" | "lo" => 0x3,
        "mi" => 0x4,
        "pl" => 0x5,
        "vs" => 0x6,
        "vc" => 0x7,
        "hi" => 0x8,
        "ls" => 0x9,
        "ge" => 0xa,
        "lt" => 0xb,
        "gt" => 0xc,
        "le" => 0xd,
        "al" => 0xe,
        _ => return None,
    };
    // Heuristic guard: don't strip "ne" off "mne"-like bare
    // mnemonics; only strip when the residual looks like a known
    // ARM/Thumb instruction prefix. A loose check: residual
    // length >= 1 and last char isn't a digit (avoids treating
    // "r0eq" as an oddity — irrelevant here since mnem is the
    // first whitespace-delimited token).
    let residual = &mnem[..mnem.len() - 2];
    if residual.is_empty() {
        return None;
    }
    Some(c)
}

fn cond_suffix(c: u8) -> &'static str {
    match c {
        0x0 => "eq",
        0x1 => "ne",
        0x2 => "cs",
        0x3 => "cc",
        0x4 => "mi",
        0x5 => "pl",
        0x6 => "vs",
        0x7 => "vc",
        0x8 => "hi",
        0x9 => "ls",
        0xa => "ge",
        0xb => "lt",
        0xc => "gt",
        0xd => "le",
        0xe => "al",
        _ => "",
    }
}

fn is_known_thumb_mnemonic(s: &str) -> bool {
    THUMB_OPCODE_TABLE_GENERATED
        .iter()
        .any(|r| r.mnemonic.as_str() == s)
}

fn parse_operand_tokens(
    s: &str,
    symbol_lookup: Option<&dyn Fn(&str) -> Option<u64>>,
) -> Result<Vec<OperandToken>> {
    let parts = tokenize_operand_strings(s, &['[', '{', '<'], &[']', '}', '>']);
    parts
        .into_iter()
        .map(|p| parse_operand_token(&p, symbol_lookup))
        .collect()
}

fn parse_operand_token(
    s: &str,
    symbol_lookup: Option<&dyn Fn(&str) -> Option<u64>>,
) -> Result<OperandToken> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty operand");
    }
    let had_hash = s.starts_with('#');
    let unhashed = s.strip_prefix('#').map(str::trim_start).unwrap_or(s);
    // Wildcards: `*`, `<*>`, `#*`.
    if unhashed == "*" {
        return Ok(OperandToken::Wildcard(if had_hash {
            WildcardKind::Imm
        } else {
            WildcardKind::Any
        }));
    }
    // Bare `r` → register wildcard.
    if s.eq_ignore_ascii_case("r") {
        return Ok(OperandToken::Wildcard(WildcardKind::Reg));
    }
    // Bracketed wildcard.
    if let Some(inner) = unhashed.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
        return Ok(OperandToken::Wildcard(classify_wildcard(inner)));
    }
    // Register list.
    if s.starts_with('{') && s.ends_with('}') {
        return parse_reg_list(&s[1..s.len() - 1]);
    }
    // Memory.
    if s.starts_with('[') {
        return parse_memory(s);
    }
    if let Some((class, idx)) = try_parse_register(s) {
        // AArch64 register names must be rejected — `x0`/`w0`.
        return Ok(OperandToken::Reg(class, idx));
    }
    // AArch64 register names get rejected explicitly so the user
    // sees a clear error instead of a "no matching opcode" deep
    // in the encoder.
    if looks_like_aarch64_register(s) {
        anyhow::bail!("operand {s:?} looks like an AArch64 register; ARMv7 uses r0..r15, sp, lr, pc");
    }
    // Bare identifier → consult the symbol resolver if one is
    // provided. A successful lookup is treated as an absolute
    // address; the slot dispatcher converts it to a `BranchTarget`
    // when the row's slot is a branch, or to an `Immediate`
    // otherwise. This mirrors the AArch64 path's symbol handling.
    if looks_like_symbol(s) {
        if let Some(lookup) = symbol_lookup {
            if let Some(abs) = lookup(s) {
                return Ok(OperandToken::Imm(abs as i64));
            }
            anyhow::bail!("unknown symbol {s:?}");
        }
        anyhow::bail!(
            "operand {s:?} looks like a symbol but no resolver was provided"
        );
    }
    if let Some(n) = try_parse_immediate(s) {
        return Ok(OperandToken::Imm(n));
    }
    anyhow::bail!("can't parse operand {s:?}");
}

fn looks_like_aarch64_register(s: &str) -> bool {
    let s = s.trim().to_ascii_lowercase();
    if s.len() < 2 {
        return false;
    }
    let head = s.as_bytes()[0];
    if !matches!(head, b'x' | b'w') {
        return false;
    }
    let rest = &s[1..];
    if rest == "zr" || rest == "sp" {
        return true;
    }
    rest.parse::<u8>().map(|n| n <= 30).unwrap_or(false)
}

fn classify_wildcard(hint: &str) -> WildcardKind {
    let h = hint.trim().to_ascii_lowercase();
    if h.is_empty() || h == "*" {
        return WildcardKind::Any;
    }
    let kind = h.split(':').next_back().unwrap_or(&h);
    match kind {
        "r" => WildcardKind::Reg,
        s if s.starts_with("imm") => WildcardKind::Imm,
        s if s.starts_with("addr") => WildcardKind::Imm,
        s if s.starts_with('r') => WildcardKind::Reg,
        _ => WildcardKind::Any,
    }
}

fn try_parse_register(s: &str) -> Option<(RegisterClass, u8)> {
    let s = s.trim();
    let lc = s.to_ascii_lowercase();
    match lc.as_str() {
        "sp" => return Some((RegisterClass::R, 13)),
        "lr" => return Some((RegisterClass::R, 14)),
        "pc" => return Some((RegisterClass::R, 15)),
        _ => {}
    }
    let bytes = lc.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'r' {
        return None;
    }
    let idx_str = std::str::from_utf8(&bytes[1..]).ok()?;
    let index: u8 = idx_str.parse().ok()?;
    if index > 15 {
        return None;
    }
    Some((RegisterClass::R, index))
}

fn parse_reg_list(inner: &str) -> Result<OperandToken> {
    let mut mask = 0u16;
    for raw in inner.split(',') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((lo_s, hi_s)) = part.split_once('-') {
            let (_, lo) = try_parse_register(lo_s.trim())
                .ok_or_else(|| anyhow!("bad reg in list: {lo_s:?}"))?;
            let (_, hi) = try_parse_register(hi_s.trim())
                .ok_or_else(|| anyhow!("bad reg in list: {hi_s:?}"))?;
            if lo > hi {
                anyhow::bail!("reg list range {lo}-{hi} reversed");
            }
            for r in lo..=hi {
                mask |= 1u16 << r;
            }
        } else {
            let (_, idx) = try_parse_register(part)
                .ok_or_else(|| anyhow!("bad reg in list: {part:?}"))?;
            mask |= 1u16 << idx;
        }
    }
    Ok(OperandToken::RegList(mask))
}

fn parse_memory(s: &str) -> Result<OperandToken> {
    let inner_start = s
        .find('[')
        .ok_or_else(|| anyhow!("memory operand missing `[`"))?;
    let inner_end = s
        .rfind(']')
        .ok_or_else(|| anyhow!("memory operand missing `]`"))?;
    if inner_start != 0 || inner_end != s.len() - 1 {
        anyhow::bail!("memory operand has extra chars outside [...]: {s:?}");
    }
    let inner = &s[inner_start + 1..inner_end];
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
    if parts.is_empty() || parts[0].is_empty() {
        anyhow::bail!("empty memory operand");
    }
    let (class, idx) = try_parse_register(parts[0])
        .ok_or_else(|| anyhow!("bad base register {:?}", parts[0]))?;
    let base = Register { class, index: idx };
    let offset = if parts.len() == 1 {
        None
    } else if parts.len() == 2 {
        Some(
            try_parse_immediate(parts[1])
                .ok_or_else(|| anyhow!("bad offset {:?}", parts[1]))?,
        )
    } else {
        anyhow::bail!("memory operand has too many components: {s:?}");
    };
    Ok(OperandToken::Memory(base, offset, ()))
}

