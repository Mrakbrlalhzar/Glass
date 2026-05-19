//! Variant index for instruction autocomplete (Phase B).
//!
//! Walks `armv8_encode::iter_opcodes()` once at first use and
//! produces a list of `Variant`s — one per opcode-table entry,
//! filtered to entries that have a usable display form (skipping
//! the small handful with non-renderable operand kinds).
//!
//! Each `Variant` carries enough metadata to drive the palette
//! autocomplete dropdown:
//!
//! - `mnemonic`: the user-facing mnemonic (e.g. `"mov"`, `"adrp"`).
//! - `slots`: a `SlotSpec` per operand position — used both to
//!   render the template (`mov <Wd>, <Wm>`) and to test whether
//!   user-typed text could fit this slot.
//! - `template`: precomputed display string, e.g.
//!   `"mov <Wd>, <Wm>"`. Shown in the dropdown verbatim.
//!
//! The slot specs are deliberately coarse — autocomplete is about
//! UX, not encoding correctness. Encoding still goes through
//! `armv8_encode::encode_instruction` once the user commits a
//! fully-concrete pattern. Anything the slot model can't classify
//! gets `SlotSpec::Other` and renders as `<arg>`; the variant
//! stays in the dropdown but contributes nothing to ranking
//! beyond the mnemonic match.

use armv8_encode::isa::aarch64::{iter_opcodes, Aarch64Opcode, Aarch64Opnd};
use std::sync::OnceLock;

/// A user-visible instruction variant. Display + slot model only;
/// the underlying opcode is referenced by index into the global
/// `iter_opcodes()` ordering when needed.
#[derive(Debug, Clone)]
pub struct Variant {
    pub mnemonic: &'static str,
    pub slots: Vec<SlotSpec>,
    pub template: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotSpec {
    /// GP register (either W- or X-class — armv8-encode picks
    /// the size at encode time from the supplied register). `sp`
    /// = true means SP / WSP is allowed in this slot.
    Gp { sp: bool },
    /// FP register, family selected by suffix in the user's input.
    FpReg,
    /// SIMD vector register (V<n>.<arr>).
    VecReg,
    /// Generic immediate (any width).
    Imm,
    /// PC-relative branch / address target — accepts a hex address.
    PcRel,
    /// Memory operand `[xN, ...]`.
    Mem,
    /// Condition code (`eq`, `ne`, …).
    Cond,
    /// System operand (sysreg name, barrier name, …).
    System,
    /// Anything else — slot is opaque, rendered as `<arg>`.
    Other,
}

impl SlotSpec {
    /// Placeholder text rendered in the template + dropdown.
    pub fn placeholder(self) -> &'static str {
        match self {
            SlotSpec::Gp { sp: false } => "x",
            SlotSpec::Gp { sp: true } => "x|sp",
            SlotSpec::FpReg => "<F>",
            SlotSpec::VecReg => "<V>",
            SlotSpec::Imm => "*",
            SlotSpec::PcRel => "*",
            SlotSpec::Mem => "[x]",
            SlotSpec::Cond => "<cond>",
            SlotSpec::System => "<sys>",
            SlotSpec::Other => "*",
        }
    }

    /// Best-effort classification of a single `Aarch64Opnd`. Two
    /// of the GP kinds can target either Wn or Xn depending on
    /// the opcode's size flag — for autocomplete purposes we
    /// expose the more permissive "either" via a heuristic on
    /// the parent opcode's mnemonic (suffix `w`/`x` hint).
    fn from_opnd(o: Aarch64Opnd, _mnem: &str) -> Self {
        use Aarch64Opnd::*;
        match o {
            Nil => SlotSpec::Other,
            Rd | Rn | Rm | Rt | Rt2 | Rs | Ra | RtSys | RmExt | RmSft => SlotSpec::Gp { sp: false },
            RdSp | RnSp => SlotSpec::Gp { sp: true },
            Pairreg => SlotSpec::Other,
            Fd | Fn | Fm | Fa | Ft | Ft2 | Sd | Sn | Sm => SlotSpec::FpReg,
            Vd | Vn | Vm | VdD1 | VnD1 | Ed | En | Em | Lvn | Lvt | LvtAl | Let => {
                SlotSpec::VecReg
            }
            Cn | Cm => SlotSpec::System,
            Idx | ImmVlsl | ImmVlsr | SimdImm | SimdImmSft | SimdFpimm | ShllImm | Imm0
            | Fpimm0 | Fpimm | Immr | Imms | Width | Imm | Uimm3Op1 | Uimm3Op2 | Uimm4
            | Uimm7 | BitNum | Exc | CcmpImm | Nzcv | Limm | Aimm | Half | Fbits | ImmMov => {
                SlotSpec::Imm
            }
            Cond | Cond1 => SlotSpec::Cond,
            AddrAdrp | AddrPcrel14 | AddrPcrel19 | AddrPcrel21 | AddrPcrel26 => SlotSpec::PcRel,
            AddrSimple | AddrRegoff | AddrSimm7 | AddrSimm9 | AddrSimm92 | AddrUimm12
            | SimdAddrSimple | SimdAddrPost => SlotSpec::Mem,
            Sysreg | Pstatefield | SysregAt | SysregDc | SysregIc | SysregTlbi | Barrier
            | BarrierIsb | Prfop | BarrierPsb => SlotSpec::System,
        }
    }
}

fn build_template(mnemonic: &str, slots: &[SlotSpec]) -> String {
    let mut out = mnemonic.to_string();
    for (i, s) in slots.iter().enumerate() {
        out.push(if i == 0 { ' ' } else { ',' });
        if i != 0 {
            out.push(' ');
        }
        out.push_str(s.placeholder());
    }
    out
}

fn make_variant(op: &Aarch64Opcode) -> Variant {
    let mnemonic = op.mnemonic();
    let slots: Vec<SlotSpec> = op
        .operands()
        .into_iter()
        .filter(|o| !matches!(o, Aarch64Opnd::Nil))
        .map(|o| SlotSpec::from_opnd(o, mnemonic))
        .collect();
    let template = build_template(mnemonic, &slots);
    Variant {
        mnemonic,
        slots,
        template,
    }
}

static VARIANTS: OnceLock<Vec<Variant>> = OnceLock::new();

/// Build (or fetch) the full variant index. Built lazily on
/// first call; subsequent calls reuse the cached `Vec`.
pub fn variants() -> &'static [Variant] {
    VARIANTS
        .get_or_init(|| iter_opcodes().map(make_variant).collect())
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_index_is_populated() {
        let v = variants();
        assert!(v.len() > 500, "expected hundreds of variants, got {}", v.len());
    }

    #[test]
    fn ret_variant_has_one_x_slot() {
        let v = variants();
        let ret = v.iter().find(|v| v.mnemonic == "ret").expect("ret in table");
        assert_eq!(ret.slots.len(), 1, "ret takes exactly Rn");
        assert!(matches!(ret.slots[0], SlotSpec::Gp { .. }));
    }

    #[test]
    fn mov_variants_exist() {
        let v = variants();
        let movs: Vec<_> = v.iter().filter(|v| v.mnemonic == "mov").collect();
        assert!(!movs.is_empty(), "mov should have variants");
    }
}
