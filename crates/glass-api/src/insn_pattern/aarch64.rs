//! AArch64 typed-assembly pattern compiler.
//!
//! Parses one or more `;`-separated AArch64 instructions and
//! drives `armv8_encode::isa::aarch64::encode_instruction` after
//! substituting placeholder values for wildcards. The wildcard
//! grammar (`<*>`, `<W>`, `<X>`, `<imm>`, …) maps to bit ranges
//! the opcode table exposes; bits a wildcard owns get cleared in
//! both mask and value so they match any value in those
//! positions. See `docs/InsnPattern.md` for the full design.
//!
//! ISA-agnostic helpers (immediate parsing, the bracket-aware
//! comma splitter, the symbol-name heuristic, `CompileOptions`)
//! live in `super::shared` and are shared with the ARMv7 path.

use anyhow::{anyhow, Context, Result};
use armv8_encode::isa::aarch64::{
    self, iter_opcodes, Aarch64Mnemonic, Aarch64Opcode, AddressingMode, DecodedOperand,
    InstructionTemplate, MemoryOffset, MemoryOperand, Register, RegisterClass,
};

use crate::bin_search::Atom;
use super::shared::{
    looks_like_symbol, tokenize_operand_strings, try_parse_immediate, CompileOptions,
    SymbolLookup,
};

/// Compile a pattern to concrete bytes (no wildcards). Errors
/// if the pattern contains any wildcard tokens. Kept for the
/// existing tests and any caller that needs raw bytes for
/// patching.
pub fn compile(pattern: &str) -> Result<Vec<u8>> {
    let atoms = compile_to_atoms(pattern)?;
    let mut out = Vec::with_capacity(atoms.len());
    for a in &atoms {
        match a {
            Atom::Mask { mask: 0xff, value } => out.push(*value),
            _ => anyhow::bail!("pattern contains wildcards; use compile_to_atoms"),
        }
    }
    Ok(out)
}

/// Compile a pattern to byte atoms with default options (address
/// = 0, no symbol resolver). The search path uses this.
pub fn compile_to_atoms(pattern: &str) -> Result<Vec<Atom>> {
    compile_to_atoms_with(pattern, &CompileOptions::default())
}

/// Compile-with-options variant. `;`-separated instructions
/// land at `options.address + 4 * i`. Wildcarded bits flow
/// through unchanged.
pub fn compile_to_atoms_with(
    pattern: &str,
    options: &CompileOptions,
) -> Result<Vec<Atom>> {
    let mut out = Vec::new();
    for (i, s) in super::shared::split_instructions(pattern) {
        let addr = options.address + (i as u64) * 4;
        let (word, mask) = compile_one(s, addr, options.symbol_lookup)
            .with_context(|| format!("instruction {} ({s:?})", i + 1))?;
        // Emit four LE byte atoms.
        let word_bytes = word.to_le_bytes();
        let mask_bytes = mask.to_le_bytes();
        for k in 0..4 {
            out.push(Atom::Mask {
                mask: mask_bytes[k],
                value: word_bytes[k] & mask_bytes[k],
            });
        }
    }
    Ok(out)
}

/// Concrete-bytes-only variant of `compile_to_atoms_with`. The
/// GUI's instruction editor calls this with the row's address
/// and the bundle's symbol resolver.
pub fn compile_at(
    pattern: &str,
    address: u64,
    symbol_lookup: Option<SymbolLookup>,
) -> Result<Vec<u8>> {
    let atoms = compile_to_atoms_with(
        pattern,
        &CompileOptions {
            address,
            symbol_lookup,
        },
    )?;
    let mut out = Vec::with_capacity(atoms.len());
    for a in &atoms {
        match a {
            Atom::Mask { mask: 0xff, value } => out.push(*value),
            _ => anyhow::bail!("pattern contains wildcards"),
        }
    }
    Ok(out)
}

/// Compile one instruction to (encoded_word, byte_mask). Mask
/// bits are 1 where bits are fixed (must match exactly) and 0
/// where they are wildcarded (match any value).
///
/// Strategy: walk every opcode-table entry whose mnemonic
/// matches and whose non-Nil operand-slot count matches the
/// token count. For each candidate, try to build placeholder
/// values appropriate to that opcode's slot kinds (wildcard
/// tokens become slot-kind-specific placeholders; concrete
/// tokens pass through as-is). Try `encode_instruction`; the
/// first successful encoding wins. We then know exactly which
/// opcode we're dealing with and use `operand_bit_ranges()` to
/// build the byte mask without a second lookup.
fn compile_one(
    s: &str,
    address: u64,
    symbol_lookup: Option<SymbolLookup>,
) -> Result<(u32, u32)> {
    use armv8_encode::isa::aarch64::Aarch64Opnd;

    let (mnem_str, rest) = split_mnemonic(s);
    let mnemonic = parse_mnemonic(mnem_str)?;
    let mut tokens = parse_operand_tokens(rest)?;
    // Resolve any Symbol tokens up-front. Failure here aborts
    // the whole instruction — we don't fall through to "maybe
    // some other opcode form takes this as something else".
    for tok in tokens.iter_mut() {
        if let OperandToken::Symbol(name) = tok {
            let Some(lookup) = symbol_lookup else {
                anyhow::bail!(
                    "operand {name:?} looks like a symbol but no resolver was provided"
                );
            };
            let abs = lookup(name).ok_or_else(|| {
                anyhow!("unknown symbol {name:?}")
            })?;
            // The exact operand kind (BranchTarget vs PageTarget
            // vs plain Immediate) depends on the opcode slot we
            // land in — we don't know it yet. Stash the absolute
            // address as an Immediate; placeholder_for_kind /
            // the opcode-matching loop below will repackage it
            // when we know which slot it fills.
            *tok = OperandToken::ResolvedSymbol(abs);
        }
    }

    // Sugar: `ret` with no operands → `ret x30`.
    if matches!(mnemonic, Aarch64Mnemonic::Ret) && tokens.is_empty() {
        tokens.push(OperandToken::Concrete(DecodedOperand::Register(Register {
            class: RegisterClass::X,
            index: 30,
        })));
    }

    let mnem_name = mnemonic.as_str();
    let mut last_err: Option<String> = None;
    // Collect candidate opcodes and rank by how well their slot
    // kinds match the user's wildcard hints. Wildcard `<imm>`
    // prefers an Imm-family slot; `<W>`/`<X>` prefer GP register
    // slots. Concrete operands constrain the encoder directly.
    let mut candidates: Vec<(i32, Aarch64Opcode)> = Vec::new();
    for op in iter_opcodes() {
        if op.mnemonic() != mnem_name {
            continue;
        }
        let slot_kinds: Vec<Aarch64Opnd> = op
            .operands()
            .into_iter()
            .filter(|o| !matches!(o, Aarch64Opnd::Nil))
            .collect();
        if slot_kinds.len() != tokens.len() {
            continue;
        }
        let mut score = 0i32;
        for (tok, kind) in tokens.iter().zip(slot_kinds.iter()) {
            match tok {
                OperandToken::Wildcard(w) => {
                    if wildcard_prefers_kind(*w, *kind) {
                        score += 1;
                    }
                }
                OperandToken::ResolvedSymbol(_) => {
                    // Symbols on a branch / PC-relative / ADRP
                    // slot are exactly what we want; reward.
                    use armv8_encode::isa::aarch64::Aarch64Opnd::*;
                    if matches!(
                        kind,
                        AddrPcrel14 | AddrPcrel19 | AddrPcrel21 | AddrPcrel26
                            | AddrAdrp
                    ) {
                        score += 2;
                    }
                }
                _ => {}
            }
        }
        candidates.push((score, *op));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, op) in candidates {
        let slot_kinds: Vec<Aarch64Opnd> = op
            .operands()
            .into_iter()
            .filter(|o| !matches!(o, Aarch64Opnd::Nil))
            .collect();
        if slot_kinds.len() != tokens.len() {
            continue;
        }

        // Build operands: concrete tokens pass through;
        // wildcards become slot-kind-specific placeholders;
        // resolved symbols become the operand kind that matches
        // the slot (BranchTarget / PageTarget / Immediate).
        let operands_result: Result<Vec<DecodedOperand>, &'static str> = tokens
            .iter()
            .zip(slot_kinds.iter())
            .map(|(tok, kind)| match tok {
                OperandToken::Concrete(op) => Ok(op.clone()),
                OperandToken::Wildcard(_) => placeholder_for_kind(*kind),
                OperandToken::Symbol(_) => {
                    // Should've been resolved up-front; treat as
                    // a compile error if we still see one.
                    Err("unresolved-symbol")
                }
                OperandToken::ResolvedSymbol(abs) => {
                    Ok(resolved_symbol_operand(*kind, *abs))
                }
            })
            .collect();
        let operands = match operands_result {
            Ok(o) => o,
            Err(e) => {
                last_err = Some(format!("no placeholder for {e}"));
                continue;
            }
        };

        let template = InstructionTemplate {
            address,
            mnemonic,
            operands,
        };
        let word = match aarch64::encode_instruction(&template) {
            Ok(w) => w,
            Err(e) => {
                last_err = Some(format!("{e:?}"));
                continue;
            }
        };
        // Sanity: the encoded word should satisfy the opcode's
        // (mask, base) — otherwise we matched a sibling entry
        // and the bit ranges won't apply correctly.
        if (word & op.mask()) != op.base_opcode() {
            continue;
        }

        // Build the byte mask. Start fully fixed and clear
        // bits owned by wildcarded operands.
        let mut mask: u32 = u32::MAX;
        let ranges = op.operand_bit_ranges();
        // Map token index → slot index in the opcode (skipping Nil).
        let slot_indices: Vec<usize> = op
            .operands()
            .iter()
            .enumerate()
            .filter(|(_, o)| !matches!(o, Aarch64Opnd::Nil))
            .map(|(i, _)| i)
            .collect();
        for (tok_idx, token) in tokens.iter().enumerate() {
            if !matches!(token, OperandToken::Wildcard(_)) {
                continue;
            }
            let Some(&slot_idx) = slot_indices.get(tok_idx) else { continue };
            let Some(slot_ranges) = ranges.get(slot_idx) else { continue };
            for r in slot_ranges {
                for bit in r.start..r.end {
                    mask &= !(1u32 << bit);
                }
            }
        }
        return Ok((word & mask, mask));
    }

    Err(anyhow!(
        "no opcode form encodes {mnem_str:?} with these operands ({} tokens) — last error: {}",
        tokens.len(),
        last_err.unwrap_or_else(|| "no candidate considered".to_string())
    ))
}

/// Does this wildcard hint prefer a slot of this kind? Used to
/// rank otherwise-tied opcode candidates so `<imm>` lands on an
/// immediate-encoded form, not the alias that hides an
/// immediate inside a different operand kind.
fn wildcard_prefers_kind(
    w: WildcardKind,
    k: armv8_encode::isa::aarch64::Aarch64Opnd,
) -> bool {
    use armv8_encode::isa::aarch64::Aarch64Opnd::*;
    match w {
        WildcardKind::Any => false,
        WildcardKind::Imm => matches!(
            k,
            Imm | Immr | Imms | Width | BitNum | Aimm | Limm | Half | Fbits | ImmMov
                | Imm0 | Uimm3Op1 | Uimm3Op2 | Uimm4 | Uimm7 | Exc | CcmpImm | Nzcv
                | AddrPcrel14 | AddrPcrel19 | AddrPcrel21 | AddrPcrel26 | AddrAdrp
        ),
        WildcardKind::RegW | WildcardKind::RegX => matches!(
            k,
            Rd | Rn | Rm | Rt | Rt2 | Rs | Ra | RtSys | RdSp | RnSp
        ),
    }
}

/// Build a placeholder `DecodedOperand` for an `Aarch64Opnd`
/// slot. The placeholder value doesn't matter — we mask it
/// out — but it must satisfy the encoder so we get the right
/// `word` to mask. Returns `Err(kind_name)` for slot kinds
/// we can't synthesise a placeholder for.
fn placeholder_for_kind(kind: armv8_encode::isa::aarch64::Aarch64Opnd) -> Result<DecodedOperand, &'static str> {
    use armv8_encode::isa::aarch64::Aarch64Opnd::*;
    let x0 = Register { class: RegisterClass::X, index: 0 };
    let w0 = Register { class: RegisterClass::W, index: 0 };
    let sp = Register { class: RegisterClass::XOrSp, index: 31 };
    let wsp = Register { class: RegisterClass::WOrSp, index: 31 };
    Ok(match kind {
        Rd | Rn | Rm | Rt | Rt2 | Rs | Ra | RtSys => DecodedOperand::Register(x0),
        RdSp | RnSp => DecodedOperand::Register(sp),
        Fd | Fn | Fm | Fa | Ft | Ft2 | Sd | Sn | Sm => DecodedOperand::Register(x0),
        Imm | Immr | Imms | Width | BitNum | Aimm | Limm | Half | Fbits | ImmMov
        | Imm0 | Uimm3Op1 | Uimm3Op2 | Uimm4 | Uimm7 | Exc | CcmpImm | Nzcv => {
            DecodedOperand::Immediate(0)
        }
        AddrPcrel14 | AddrPcrel19 | AddrPcrel21 | AddrPcrel26 => {
            DecodedOperand::BranchTarget(0)
        }
        AddrAdrp => DecodedOperand::PageTarget(0),
        AddrSimple => DecodedOperand::Memory(MemoryOperand {
            base: sp,
            offset: MemoryOffset::None,
            mode: AddressingMode::Offset,
        }),
        AddrSimm7 | AddrSimm9 | AddrSimm92 | AddrUimm12 | AddrRegoff => {
            DecodedOperand::Memory(MemoryOperand {
                base: sp,
                offset: MemoryOffset::Immediate(0),
                mode: AddressingMode::Offset,
            })
        }
        Cond | Cond1 => DecodedOperand::Condition("eq"),
        _ => {
            let _ = (w0, wsp); // silence unused-bindings in the fallback path
            return Err("unsupported-operand-kind");
        }
    })
}

#[derive(Debug, Clone, Copy)]
enum WildcardKind {
    /// `<*>` — any operand kind. The slot's real kind is taken
    /// from the opcode at compile time.
    Any,
    /// `<W>`, `<Wd>`, `<Wn>`, … — a W-class GP register.
    RegW,
    /// `<X>`, `<Xd>`, `<Xn>`, … — an X-class GP register.
    RegX,
    /// `<imm>`, `<imm12>`, etc. — any immediate.
    Imm,
}

#[derive(Debug, Clone)]
enum OperandToken {
    Concrete(DecodedOperand),
    Wildcard(WildcardKind),
    /// Bare identifier — looks like a symbol name (`foo`,
    /// `decode_packet`, `glass::main`). Resolved up-front in
    /// `compile_one`; the parsed token only carries the name.
    Symbol(String),
    /// Symbol that's been resolved to an absolute address. The
    /// opcode-matching loop wraps it as a BranchTarget,
    /// PageTarget, or Immediate depending on the slot kind it
    /// lands in.
    ResolvedSymbol(u64),
}

/// Wrap a resolved-symbol absolute address into the right
/// `DecodedOperand` variant for the opcode slot it's landing
/// in. Branch / PC-relative slots want `BranchTarget`; ADRP
/// wants `PageTarget`; anything else (e.g. an immediate slot
/// the user typed a label into for whatever reason) gets a
/// plain `Immediate` and the encoder validates from there.
fn resolved_symbol_operand(
    kind: armv8_encode::isa::aarch64::Aarch64Opnd,
    abs: u64,
) -> DecodedOperand {
    use armv8_encode::isa::aarch64::Aarch64Opnd::*;
    match kind {
        AddrPcrel14 | AddrPcrel19 | AddrPcrel21 | AddrPcrel26 => {
            DecodedOperand::BranchTarget(abs)
        }
        AddrAdrp => DecodedOperand::PageTarget(abs),
        _ => DecodedOperand::Immediate(abs as i64),
    }
}

fn split_mnemonic(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], s[idx..].trim()),
        None => (s, ""),
    }
}

/// `Aarch64Mnemonic::parse` takes `&'static str`. User input is
/// owned. For unknown mnemonics it returns `Other(name)` which
/// also keeps the static lifetime — we leak the input string in
/// that case. Memory growth is bounded by the set of distinct
/// mnemonic spellings the user types per process.
fn parse_mnemonic(name: &str) -> Result<Aarch64Mnemonic> {
    let lower = name.to_ascii_lowercase();
    // Common mnemonics get a static match arm so we don't leak.
    let known: Option<Aarch64Mnemonic> = match lower.as_str() {
        "add" => Some(Aarch64Mnemonic::Add),
        "adds" => Some(Aarch64Mnemonic::Adds),
        "and" => Some(Aarch64Mnemonic::And),
        "adr" => Some(Aarch64Mnemonic::Adr),
        "adrp" => Some(Aarch64Mnemonic::Adrp),
        "b" => Some(Aarch64Mnemonic::B),
        "bl" => Some(Aarch64Mnemonic::Bl),
        "blr" => Some(Aarch64Mnemonic::Blr),
        "br" => Some(Aarch64Mnemonic::Br),
        "brk" => Some(Aarch64Mnemonic::Brk),
        "cbnz" => Some(Aarch64Mnemonic::Cbnz),
        "cbz" => Some(Aarch64Mnemonic::Cbz),
        "ccmp" => Some(Aarch64Mnemonic::Ccmp),
        "cmn" => Some(Aarch64Mnemonic::Cmn),
        "cmp" => Some(Aarch64Mnemonic::Cmp),
        "csel" => Some(Aarch64Mnemonic::Csel),
        "eor" => Some(Aarch64Mnemonic::Eor),
        "ldp" => Some(Aarch64Mnemonic::Ldp),
        "ldr" => Some(Aarch64Mnemonic::Ldr),
        "lsl" => Some(Aarch64Mnemonic::Lsl),
        "lsr" => Some(Aarch64Mnemonic::Lsr),
        "madd" => Some(Aarch64Mnemonic::Madd),
        "mov" => Some(Aarch64Mnemonic::Mov),
        "movk" => Some(Aarch64Mnemonic::Movk),
        "msub" => Some(Aarch64Mnemonic::Msub),
        "nop" => Some(Aarch64Mnemonic::Nop),
        "ret" => Some(Aarch64Mnemonic::Ret),
        "stp" => Some(Aarch64Mnemonic::Stp),
        "str" => Some(Aarch64Mnemonic::Str),
        "sub" => Some(Aarch64Mnemonic::Sub),
        "subs" => Some(Aarch64Mnemonic::Subs),
        "tbnz" => Some(Aarch64Mnemonic::Tbnz),
        "tbz" => Some(Aarch64Mnemonic::Tbz),
        "ubfx" => Some(Aarch64Mnemonic::Ubfx),
        _ => None,
    };
    if let Some(m) = known {
        return Ok(m);
    }
    // Fall back to the upstream `parse` for anything we didn't
    // enumerate (covers the B.cond family and similar). We leak
    // the lowercased name so the upstream `&'static str`
    // contract is satisfied; growth is bounded by the variety
    // of mnemonics the user types.
    let leaked: &'static str = Box::leak(lower.into_boxed_str());
    Ok(Aarch64Mnemonic::parse(leaked))
}

fn parse_operand_tokens(s: &str) -> Result<Vec<OperandToken>> {
    let parts = tokenize_operand_strings(s, &['[', '<'], &[']', '>']);
    parts.into_iter().map(|p| parse_operand_token(&p)).collect()
}

fn parse_operand_token(s: &str) -> Result<OperandToken> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty operand");
    }
    // Shorthand wildcards:
    //   `*` (or `#*`)  → any-slot wildcard
    //   bare `x`       → X-class register wildcard
    //   bare `w`       → W-class register wildcard
    // These are intuitive shortcuts for the bracketed forms
    // `<*>`, `<X>`, `<W>`. The bracketed forms still parse
    // and remain useful for captures (`<Xd:x>` in Phase D).
    let had_hash = s.starts_with('#');
    let unhashed = s.strip_prefix('#').map(str::trim_start).unwrap_or(s);
    if unhashed == "*" {
        // `#*` carries an immediate-kind hint from the sigil;
        // bare `*` is fully kind-agnostic.
        return Ok(OperandToken::Wildcard(if had_hash {
            WildcardKind::Imm
        } else {
            WildcardKind::Any
        }));
    }
    match s.to_ascii_lowercase().as_str() {
        "x" => return Ok(OperandToken::Wildcard(WildcardKind::RegX)),
        "w" => return Ok(OperandToken::Wildcard(WildcardKind::RegW)),
        _ => {}
    }
    // Bracketed wildcard: `<...>` or `#<...>`.
    let unwild = unhashed
        .strip_prefix('<')
        .and_then(|t| t.strip_suffix('>'));
    if let Some(inner) = unwild {
        return Ok(OperandToken::Wildcard(classify_wildcard(inner)));
    }
    if s.starts_with('[') {
        return Ok(OperandToken::Concrete(parse_memory(s)?));
    }
    if let Some(reg) = try_parse_register(s) {
        return Ok(OperandToken::Concrete(DecodedOperand::Register(reg)));
    }
    if let Some(n) = try_parse_immediate(s) {
        return Ok(OperandToken::Concrete(DecodedOperand::Immediate(n)));
    }
    // Last-chance: an identifier that looks like a symbol name.
    // The compile_one pass will try to resolve it via the
    // caller-supplied lookup and fail if there's no resolver.
    if looks_like_symbol(s) {
        return Ok(OperandToken::Symbol(s.to_string()));
    }
    anyhow::bail!("can't parse operand {s:?}")
}

fn classify_wildcard(hint: &str) -> WildcardKind {
    let h = hint.trim().to_ascii_lowercase();
    if h.is_empty() || h == "*" {
        return WildcardKind::Any;
    }
    // Strip capture name prefix: `xd:x` → kind hint "x".
    let kind_hint = h.split(':').next_back().unwrap_or(&h);
    // Look at first character + common suffixes.
    match kind_hint {
        "w" | "wd" | "wn" | "wm" | "wt" | "wa" | "ws" => WildcardKind::RegW,
        "x" | "xd" | "xn" | "xm" | "xt" | "xa" | "xs" => WildcardKind::RegX,
        s if s.starts_with("imm") => WildcardKind::Imm,
        s if s.starts_with("addr") => WildcardKind::Imm,
        // Single starting letter heuristic for less-common spellings.
        s if s.starts_with('w') => WildcardKind::RegW,
        s if s.starts_with('x') => WildcardKind::RegX,
        _ => WildcardKind::Any,
    }
}

fn try_parse_register(s: &str) -> Option<Register> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("sp") {
        return Some(Register { class: RegisterClass::XOrSp, index: 31 });
    }
    if s.eq_ignore_ascii_case("wsp") {
        return Some(Register { class: RegisterClass::WOrSp, index: 31 });
    }
    if s.eq_ignore_ascii_case("xzr") {
        return Some(Register { class: RegisterClass::X, index: 31 });
    }
    if s.eq_ignore_ascii_case("wzr") {
        return Some(Register { class: RegisterClass::W, index: 31 });
    }
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let class = match bytes[0] | 0x20 {
        b'w' => RegisterClass::W,
        b'x' => RegisterClass::X,
        _ => return None,
    };
    let idx_str = std::str::from_utf8(&bytes[1..]).ok()?;
    let index: u8 = idx_str.parse().ok()?;
    if index > 30 {
        return None;
    }
    Some(Register { class, index })
}

fn parse_memory(s: &str) -> Result<DecodedOperand> {
    // Phase A supports two forms:
    //   [reg]           — base only.
    //   [reg, #imm]     — base + signed immediate offset.
    //
    // Pre-/post-index (`[reg, #imm]!`, `[reg], #imm`) and
    // register-offset forms ([reg, reg]) wait for Phase B/C.
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
    let base = try_parse_register(parts[0])
        .ok_or_else(|| anyhow!("bad base register {:?}", parts[0]))?;
    if base.class != RegisterClass::X && base.class != RegisterClass::XOrSp {
        anyhow::bail!("memory base must be X / XSP, got {:?}", parts[0]);
    }
    let offset = if parts.len() == 1 {
        MemoryOffset::None
    } else if parts.len() == 2 {
        let imm = try_parse_immediate(parts[1])
            .ok_or_else(|| anyhow!("bad offset {:?}", parts[1]))?;
        MemoryOffset::Immediate(imm)
    } else {
        anyhow::bail!("memory operand has too many components: {s:?}");
    };
    Ok(DecodedOperand::Memory(MemoryOperand {
        base,
        offset,
        mode: AddressingMode::Offset,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers() {
        assert_eq!(
            try_parse_register("w0"),
            Some(Register { class: RegisterClass::W, index: 0 })
        );
        assert_eq!(
            try_parse_register("X29"),
            Some(Register { class: RegisterClass::X, index: 29 })
        );
        assert_eq!(
            try_parse_register("sp"),
            Some(Register { class: RegisterClass::XOrSp, index: 31 })
        );
        assert_eq!(
            try_parse_register("xzr"),
            Some(Register { class: RegisterClass::X, index: 31 })
        );
        assert!(try_parse_register("z0").is_none());
        assert!(try_parse_register("w99").is_none());
    }

    #[test]
    fn immediates() {
        assert_eq!(try_parse_immediate("#1"), Some(1));
        assert_eq!(try_parse_immediate("#0x10"), Some(16));
        assert_eq!(try_parse_immediate("16"), Some(16));
        assert_eq!(try_parse_immediate("-1"), Some(-1));
        assert_eq!(try_parse_immediate("#-0x10"), Some(-16));
    }

    #[test]
    fn compile_ret() {
        // Bare `ret` is sugared to `ret x30` by `compile_one`.
        let bytes = compile("ret").unwrap();
        assert_eq!(bytes, vec![0xc0, 0x03, 0x5f, 0xd6]);
        // Explicit form parses the same.
        let bytes2 = compile("ret x30").unwrap();
        assert_eq!(bytes2, bytes);
    }

    #[test]
    fn compile_mov_w0_zero() {
        let bytes = compile("mov w0, #0").unwrap();
        assert_eq!(bytes, vec![0x00, 0x00, 0x80, 0x52]);
    }

    #[test]
    fn compile_two_insns() {
        let bytes = compile("mov x0, #0 ; ret").unwrap();
        assert_eq!(
            bytes,
            vec![0x00, 0x00, 0x80, 0xd2, 0xc0, 0x03, 0x5f, 0xd6]
        );
    }

    #[test]
    fn wildcard_any_clears_mask_bits() {
        let atoms = compile_to_atoms("adrp x1, <*>").unwrap();
        assert_eq!(atoms.len(), 4, "one insn = 4 byte atoms");
        let has_partial = atoms
            .iter()
            .any(|a| matches!(a, Atom::Mask { mask, .. } if *mask != 0xff));
        assert!(has_partial, "wildcard should produce at least one partial-mask byte");
    }

    #[test]
    fn fully_concrete_round_trips_through_compile() {
        let bytes = compile("mov w0, #1").unwrap();
        assert_eq!(bytes, vec![0x20, 0x00, 0x80, 0x52]);
    }

    #[test]
    fn wildcard_reg_w() {
        let atoms = compile_to_atoms("mov <W>, #1").unwrap();
        assert_eq!(atoms.len(), 4);
        match &atoms[0] {
            Atom::Mask { mask, .. } => {
                assert_eq!(mask & 0x1f, 0, "low 5 bits (Rd) should be wildcarded");
            }
            _ => panic!("expected Mask atom"),
        }
    }

    #[test]
    fn bl_resolves_symbol_to_pc_relative() {
        let lookup = |name: &str| -> Option<u64> {
            (name == "decode_packet").then_some(0x100000010)
        };
        let bytes = compile_at(
            "bl decode_packet",
            0x100000000,
            Some(&lookup),
        )
        .expect("compile bl symbol");
        let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(word & 0xfc000000, 0x94000000, "BL opcode bits");
        assert_eq!(word & 0x03ffffff, 4, "delta-in-words");
    }

    #[test]
    fn unknown_symbol_errors() {
        let lookup = |_: &str| -> Option<u64> { None };
        let err = compile_at("bl mystery_func", 0, Some(&lookup))
            .expect_err("should fail to resolve");
        assert!(format!("{err:#}").contains("mystery_func"));
    }

    #[test]
    fn symbol_with_no_resolver_errors() {
        let err = compile_at("bl decode_packet", 0, None).expect_err("no resolver");
        assert!(format!("{err:#}").contains("resolver"));
    }
}
