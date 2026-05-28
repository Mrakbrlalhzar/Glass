//! Lightweight pretty-printer for ARMv7 (A32) and Thumb (T32)
//! decoded instructions.
//!
//! The upstream `armv8-encode` crate exposes ARMv7 instructions as
//! a typed `(mnemonic, operands)` pair where each operand is one
//! of [`armv8_encode::isa::armv7::operand::DecodedOperand`] variants.
//! This module turns that into a single human-readable line for the
//! listing.
//!
//! Bootstrap quality: we render the mnemonic from the matched row,
//! then each operand in its natural form (`r0`, `#42`, `0x1234`,
//! `{r4, r5, lr}`, …). `OpaqueBits` operands are suppressed unless
//! no other operand is present, in which case we emit a `;` carrying
//! the raw bits — this mirrors what binutils does when the format
//! string couldn't be fully decoded. Polishing the rendering
//! (addressing modes, shifted-register notation, condition-code
//! suffixes pulled out of the mnemonic) is a follow-up.

use armv8_encode::isa::armv7::arm::sweep::ArmDecodedInstruction;
use armv8_encode::isa::armv7::operand::{DecodedOperand, Register, RegisterClass};
use armv8_encode::isa::armv7::sweep::ThumbDecodedInstruction;

pub fn format_arm(insn: &ArmDecodedInstruction) -> String {
    let mnem = insn.mnemonic_name();
    let ops = render_operands(&insn.operands);
    if ops.is_empty() {
        mnem.to_string()
    } else {
        format!("{mnem} {ops}")
    }
}

pub fn format_thumb(insn: &ThumbDecodedInstruction) -> String {
    let mnem = insn.mnemonic_name();
    let ops = render_operands(&insn.operands);
    if ops.is_empty() {
        mnem.to_string()
    } else {
        format!("{mnem} {ops}")
    }
}

fn render_operands(ops: &[DecodedOperand]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for op in ops {
        let Some(rendered) = render_operand(op) else { continue };
        parts.push(rendered);
    }
    parts.join(", ")
}

fn render_operand(op: &DecodedOperand) -> Option<String> {
    match op {
        DecodedOperand::Register(r) => Some(register_name(r)),
        DecodedOperand::Immediate(v) => {
            if v.is_negative() {
                Some(format!("#-0x{:x}", v.wrapping_neg() as u64))
            } else if (0..=0x1000).contains(v) {
                Some(format!("#{v}"))
            } else {
                Some(format!("#0x{:x}", v))
            }
        }
        DecodedOperand::BranchTarget(addr) | DecodedOperand::PcRelative(addr) => {
            Some(format!("0x{addr:x}"))
        }
        DecodedOperand::RegisterList(bits) => Some(format_register_list(*bits)),
        DecodedOperand::Condition(c) => Some(condition_name(*c).to_string()),
        // OpaqueBits is the upstream's "couldn't fully decode this"
        // signal. Suppress it from the rendered line — the
        // mnemonic + already-decoded operands carry enough info,
        // and dumping raw bits adds noise. Encoding still
        // round-trips correctly because the OpaqueBits stays on
        // the DecodedInsn.
        DecodedOperand::OpaqueBits { .. } => None,
    }
}

fn register_name(r: &Register) -> String {
    match (r.class, r.index) {
        (RegisterClass::R | RegisterClass::Low, 13) => "sp".to_string(),
        (RegisterClass::R | RegisterClass::Low, 14) => "lr".to_string(),
        (RegisterClass::R | RegisterClass::Low, 15) => "pc".to_string(),
        (RegisterClass::R | RegisterClass::Low, n) => format!("r{n}"),
        (RegisterClass::S, n) => format!("s{n}"),
        (RegisterClass::D, n) => format!("d{n}"),
        (RegisterClass::Q, n) => format!("q{n}"),
    }
}

fn format_register_list(bits: u16) -> String {
    let mut names = Vec::new();
    for i in 0..16u8 {
        if (bits & (1 << i)) != 0 {
            names.push(match i {
                13 => "sp".to_string(),
                14 => "lr".to_string(),
                15 => "pc".to_string(),
                n => format!("r{n}"),
            });
        }
    }
    format!("{{{}}}", names.join(", "))
}

fn condition_name(c: u8) -> &'static str {
    match c {
        0 => "eq",
        1 => "ne",
        2 => "cs",
        3 => "cc",
        4 => "mi",
        5 => "pl",
        6 => "vs",
        7 => "vc",
        8 => "hi",
        9 => "ls",
        10 => "ge",
        11 => "lt",
        12 => "gt",
        13 => "le",
        14 => "al",
        _ => "??",
    }
}
