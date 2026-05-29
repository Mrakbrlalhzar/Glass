//! Architecture-neutral instruction facade.
//!
//! [`DecodedInsn`] wraps the three per-ISA decoded-instruction types
//! exposed by `armv8-encode` (AArch64, ARM-mode, Thumb-mode) so the
//! UI / analysis layers can operate on a single enum and stay
//! ISA-agnostic. The enum implements [`armv8_encode::mc::InstructionInfo`]
//! by forwarding to the inner variant, which unlocks the upstream
//! `mc::build_cfg` for ARMv7 with no extra code.
//!
//! Beyond `InstructionInfo`, the type carries the small bundle of
//! inherent accessors Glass's listing / xref / CFG code historically
//! pattern-matched into AArch64's `DecodedInstruction` for. The
//! AArch64 arm of each accessor is a relocated copy of the helper
//! that previously lived in `glass-ui/src/listing_model.rs` and
//! `glass-ui/src/xref.rs` (which were textually duplicated); behavior
//! is identical so AArch64 listings render byte-for-byte the same as
//! before.
//!
//! The ARMv7 arms are bootstrap-quality. `format_text` reuses the
//! upstream operand model and our own pretty-printer; the analysis
//! accessors (`branch_target`, `first_imm`, register uses) consult
//! the same `DecodedOperand` variants the upstream decoder emits.
//! ADRP+ADD-style page-base fusion and `movw+movt` reconstruction
//! are AArch64-only for this pass — ARMv7's PC-relative literal-pool
//! references decode straight to `DecodedOperand::PcRelative(addr)`
//! and need no fusion.

use armv8_encode::isa::aarch64::DecodedInstruction as Aarch64Insn;
use armv8_encode::isa::armv7::arm::sweep::ArmDecodedInstruction;
use armv8_encode::isa::armv7::sweep::ThumbDecodedInstruction;
use armv8_encode::mc::{ControlFlow, InstructionInfo};

use crate::format as aarch64_fmt;

/// Architecture-neutral decoded instruction.
#[derive(Debug, Clone)]
pub enum DecodedInsn {
    Aarch64(Aarch64Insn),
    Arm(ArmDecodedInstruction),
    Thumb(ThumbDecodedInstruction),
}

impl InstructionInfo for DecodedInsn {
    fn address(&self) -> u64 {
        match self {
            DecodedInsn::Aarch64(i) => i.address(),
            DecodedInsn::Arm(i) => i.address(),
            DecodedInsn::Thumb(i) => i.address(),
        }
    }
    fn size(&self) -> u64 {
        match self {
            DecodedInsn::Aarch64(i) => i.size(),
            DecodedInsn::Arm(i) => i.size(),
            DecodedInsn::Thumb(i) => i.size(),
        }
    }
    fn control_flow(&self) -> ControlFlow {
        match self {
            DecodedInsn::Aarch64(i) => i.control_flow(),
            DecodedInsn::Arm(i) => i.control_flow(),
            DecodedInsn::Thumb(i) => i.control_flow(),
        }
    }
}

/// Distinguishes register kinds across ISAs so the UI's
/// "highlight all uses of this register" feature stays scoped
/// correctly (an `x5` use shouldn't highlight an `r5` use).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RegKind {
    /// AArch64 64-bit GP (`x0..x30`, `sp`).
    AArch64Gpr64,
    /// AArch64 32-bit GP (`w0..w30`, `wsp`).
    AArch64Gpr32,
    /// ARMv7 GP (`r0..r15`).
    ArmGpr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RegRef {
    pub kind: RegKind,
    pub index: u8,
}

impl DecodedInsn {
    /// Encoded width in bytes. Wraps `InstructionInfo::size` for
    /// callers that don't want to bring the trait into scope.
    pub fn width_bytes(&self) -> usize {
        InstructionInfo::size(self) as usize
    }

    /// True if this instruction is a 16-bit Thumb-1 NOP
    /// (encoded as `0xbf 0x00`). The editor uses this to decide
    /// whether a 2-byte Thumb-1 instruction can be grown to a
    /// 4-byte form by absorbing the following slot.
    pub fn is_thumb1_nop(&self) -> bool {
        use armv8_encode::isa::armv7::table::ThumbWidth;
        use armv8_encode::isa::armv7::table_generated::ThumbMnemonicGenerated;
        match self {
            DecodedInsn::Thumb(i) => {
                matches!(i.width, ThumbWidth::Halfword)
                    && i.mnemonic == ThumbMnemonicGenerated::Nop
            }
            _ => false,
        }
    }

    /// Raw little-endian bytes of this instruction. Returns up to 4
    /// bytes; AArch64 and ARM-mode always yield exactly 4, Thumb
    /// yields 2 or 4 depending on width.
    pub fn raw_bytes(&self) -> Vec<u8> {
        match self {
            DecodedInsn::Aarch64(i) => i.word.to_le_bytes().to_vec(),
            DecodedInsn::Arm(i) => i.word.to_le_bytes().to_vec(),
            DecodedInsn::Thumb(i) => {
                use armv8_encode::isa::armv7::table::ThumbWidth;
                match i.width {
                    ThumbWidth::Halfword => {
                        // 16-bit Thumb sits in the low half.
                        let hw = i.word as u16;
                        hw.to_le_bytes().to_vec()
                    }
                    ThumbWidth::Word => {
                        // 32-bit Thumb: hw1 in the high half, hw2 in
                        // the low half (upstream convention).
                        let hw1 = ((i.word >> 16) & 0xFFFF) as u16;
                        let hw2 = (i.word & 0xFFFF) as u16;
                        let mut out = Vec::with_capacity(4);
                        out.extend_from_slice(&hw1.to_le_bytes());
                        out.extend_from_slice(&hw2.to_le_bytes());
                        out
                    }
                }
            }
        }
    }

    /// Pretty-print mnemonic + operands. The result is the same text
    /// Glass listings have always shown for AArch64. For ARMv7 the
    /// formatter uses the upstream mnemonic and the `Debug` projection
    /// of each operand — readable but not yet polished.
    pub fn format_text(&self) -> String {
        match self {
            DecodedInsn::Aarch64(i) => {
                let mnem = aarch64_fmt::mnemonic_chunk(i).text;
                let ops: String = aarch64_fmt::operands_chunks(i)
                    .into_iter()
                    .map(|c| c.text)
                    .collect();
                if ops.is_empty() {
                    mnem
                } else {
                    format!("{mnem} {ops}")
                }
            }
            DecodedInsn::Arm(i) => crate::arm_format::format_arm(i),
            DecodedInsn::Thumb(i) => crate::arm_format::format_thumb(i),
        }
    }

    /// Every general-purpose register the instruction touches, in
    /// the order they appear in the operand list. Used by the UI to
    /// highlight uses of the same register across rows.
    pub fn gp_register_uses(&self) -> Vec<RegRef> {
        use armv8_encode::isa::aarch64 as a64;
        use armv8_encode::isa::armv7::operand as a7;
        let mut out = Vec::new();
        match self {
            DecodedInsn::Aarch64(i) => {
                for op in &i.operands {
                    if let a64::DecodedOperand::Register(r) = op {
                        match r.class {
                            a64::RegisterClass::X | a64::RegisterClass::XOrSp => {
                                out.push(RegRef { kind: RegKind::AArch64Gpr64, index: r.index })
                            }
                            a64::RegisterClass::W | a64::RegisterClass::WOrSp => {
                                out.push(RegRef { kind: RegKind::AArch64Gpr32, index: r.index })
                            }
                            _ => {}
                        }
                    }
                }
            }
            DecodedInsn::Arm(i) => {
                for op in &i.operands {
                    if let a7::DecodedOperand::Register(r) = op {
                        if matches!(r.class, a7::RegisterClass::R | a7::RegisterClass::Low) {
                            out.push(RegRef { kind: RegKind::ArmGpr, index: r.index });
                        }
                    }
                }
            }
            DecodedInsn::Thumb(i) => {
                for op in &i.operands {
                    if let a7::DecodedOperand::Register(r) = op {
                        if matches!(r.class, a7::RegisterClass::R | a7::RegisterClass::Low) {
                            out.push(RegRef { kind: RegKind::ArmGpr, index: r.index });
                        }
                    }
                }
            }
        }
        out
    }

    /// First immediate operand, normalised to `i64`. Matches the
    /// behavior of the helper that used to live in
    /// `listing_model.rs`: `Immediate` → value; `UnsignedImmediate`
    /// → cast; `ShiftedImmediate` → `value << shift`.
    pub fn first_imm(&self) -> Option<i64> {
        use armv8_encode::isa::aarch64 as a64;
        use armv8_encode::isa::armv7::operand as a7;
        match self {
            DecodedInsn::Aarch64(i) => {
                for op in &i.operands {
                    match op {
                        a64::DecodedOperand::Immediate(v) => return Some(*v),
                        a64::DecodedOperand::UnsignedImmediate(v) => return Some(*v as i64),
                        a64::DecodedOperand::ShiftedImmediate(s) => {
                            return Some(s.value.wrapping_shl(s.shift as u32))
                        }
                        _ => {}
                    }
                }
                None
            }
            DecodedInsn::Arm(i) => {
                for op in &i.operands {
                    if let a7::DecodedOperand::Immediate(v) = op {
                        return Some(*v);
                    }
                }
                None
            }
            DecodedInsn::Thumb(i) => {
                for op in &i.operands {
                    if let a7::DecodedOperand::Immediate(v) = op {
                        return Some(*v);
                    }
                }
                None
            }
        }
    }

    /// Resolved direct-branch target, if the instruction encodes
    /// one. ARMv7's `BranchTarget` variant gives us the absolute
    /// address directly; AArch64 has both `BranchTarget` and
    /// `PageTarget` (the latter from `ADRP`), and we prefer
    /// `BranchTarget` because `PageTarget` is reachable via the
    /// dedicated [`Self::pcrel_target`] accessor.
    pub fn branch_target(&self) -> Option<u64> {
        use armv8_encode::isa::aarch64 as a64;
        use armv8_encode::isa::armv7::operand as a7;
        match self {
            DecodedInsn::Aarch64(i) => {
                for op in &i.operands {
                    if let a64::DecodedOperand::BranchTarget(a) = op {
                        return Some(*a);
                    }
                }
                None
            }
            DecodedInsn::Arm(i) => {
                for op in &i.operands {
                    if let a7::DecodedOperand::BranchTarget(a) = op {
                        return Some(*a);
                    }
                }
                None
            }
            DecodedInsn::Thumb(i) => {
                for op in &i.operands {
                    if let a7::DecodedOperand::BranchTarget(a) = op {
                        return Some(*a);
                    }
                }
                None
            }
        }
    }

    /// PC-relative literal-pool or page address target. AArch64
    /// `ADR`/`ADRP` emit `PageTarget`; ARMv7 Thumb `ldr Rt,[pc,#imm]`
    /// emits `PcRelative`. ARM-mode literal pools currently come
    /// through as plain memory operands and aren't covered here.
    pub fn pcrel_target(&self) -> Option<u64> {
        use armv8_encode::isa::aarch64 as a64;
        use armv8_encode::isa::armv7::operand as a7;
        match self {
            DecodedInsn::Aarch64(i) => {
                for op in &i.operands {
                    if let a64::DecodedOperand::PageTarget(a) = op {
                        return Some(*a);
                    }
                }
                None
            }
            DecodedInsn::Arm(_) => None,
            DecodedInsn::Thumb(i) => {
                for op in &i.operands {
                    if let a7::DecodedOperand::PcRelative(a) = op {
                        return Some(*a);
                    }
                }
                None
            }
        }
    }

    /// Destination general-purpose register if there is one. For
    /// AArch64 this is the first X-class register; for ARMv7 it's
    /// the first `R`/`Low`-class register. Used by the listing's
    /// ADRP+ADD page-base tracker (AArch64 only) and by the arrow
    /// renderer.
    pub fn dest_register(&self) -> Option<RegRef> {
        self.gp_register_uses().into_iter().next()
    }

    /// Direct AArch64 view — `None` for ARMv7 variants. Lets the
    /// few remaining AArch64-only code paths (insn-pattern matcher,
    /// page-base fusion) keep their existing pattern matches without
    /// re-implementing them through the facade.
    pub fn as_aarch64(&self) -> Option<&Aarch64Insn> {
        match self {
            DecodedInsn::Aarch64(i) => Some(i),
            _ => None,
        }
    }

    /// Recognise ARMv7 instruction shapes that the listing's
    /// macro-fusion tracker cares about (currently `movw`/`movt`
    /// pair tracking). Returns the destination register and the
    /// 16-bit immediate.
    ///
    /// `movw Rd, #imm16` zero-extends into the low 16 bits of `Rd`;
    /// `movt Rd, #imm16` writes the imm into the high 16 bits of `Rd`
    /// without disturbing the low half. The pair builds a 32-bit
    /// absolute constant that's almost always a pointer into
    /// rodata — so the listing renderer wants to follow it.
    pub fn armv7_movw(&self) -> Option<(u8, u16)> {
        use armv8_encode::isa::armv7::arm::table_generated::ArmMnemonicGenerated as ArmM;
        use armv8_encode::isa::armv7::table_generated::ThumbMnemonicGenerated as ThumbM;
        match self {
            DecodedInsn::Arm(i) if i.mnemonic == ArmM::Movw => {
                armv7_movw_movt_operands(self)
            }
            DecodedInsn::Thumb(i) if i.mnemonic == ThumbM::Movw => {
                armv7_movw_movt_operands(self)
            }
            _ => None,
        }
    }

    /// Same shape as `armv7_movw` but for `movt`. The returned `Rd`
    /// must be matched against the most recent `movw` target before
    /// the pair can be fused into a 32-bit constant.
    pub fn armv7_movt(&self) -> Option<(u8, u16)> {
        use armv8_encode::isa::armv7::arm::table_generated::ArmMnemonicGenerated as ArmM;
        use armv8_encode::isa::armv7::table_generated::ThumbMnemonicGenerated as ThumbM;
        match self {
            DecodedInsn::Arm(i) if i.mnemonic == ArmM::Movt => {
                armv7_movw_movt_operands(self)
            }
            DecodedInsn::Thumb(i) if i.mnemonic == ThumbM::Movt => {
                armv7_movw_movt_operands(self)
            }
            _ => None,
        }
    }
}

/// Extract `(Rd, imm16)` from an already-classified ARMv7
/// `movw` / `movt` instruction. Both forms decode to operands
/// `[Register(Rd), Immediate(imm)]`; we just project them out.
fn armv7_movw_movt_operands(insn: &DecodedInsn) -> Option<(u8, u16)> {
    let dest = insn.dest_register()?;
    if dest.kind != RegKind::ArmGpr {
        return None;
    }
    let imm = insn.first_imm()?;
    // The decoder emits a non-negative i64 here; mask to 16 bits
    // to be safe against any signed projection.
    Some((dest.index, (imm as u32 & 0xFFFF) as u16))
}

/// Result of a successful fusion-pair completion. Carries enough
/// info for both consumers:
///   * `target` — the listing comment renderer ("; \"foo\"") and
///     the xref index both record this.
///   * `source_register` — the listing's per-row retro-label
///     (the ADRP row gets relabelled with the destination section
///     when the pair resolves to a different section). Equals the
///     destination register for ARMv7 movw+movt (since the pair
///     consumes its own previous write).
#[derive(Debug, Clone, Copy)]
pub struct FusionTarget {
    pub target: u64,
    pub source_register: u8,
}

/// Stateful tracker for cross-instruction fusion idioms used to
/// resolve "what address does this pair of instructions actually
/// reference?". Walk decoded instructions in source order via
/// `observe(insn) -> Option<FusionTarget>`.
///
/// Supported idioms:
///   * AArch64 `ADRP Xd, page ; ADD Xd, Xs, #imm`. Returns
///     target = page + imm.
///   * ARMv7 (Thumb-2 / A32) `movw Rd, #lo16 ; movt Rd, #hi16`.
///     Returns target = (hi16 << 16) | lo16.
///
/// Any non-completing write to a tracked register invalidates
/// that slot — same conservative rule both call sites used before.
///
/// AArch64 state slots are unused for ARMv7 inputs and vice
/// versa. Either ISA can produce mixed observations without
/// confusing the other's state.
#[derive(Debug, Default, Clone)]
pub struct PageBaseTracker {
    aarch64_pages: [Option<u64>; 32],
    armv7_movw_lo: [Option<u16>; 16],
}

impl PageBaseTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one instruction in source order. Returns the
    /// fused absolute address when this instruction completes a
    /// known pair; updates internal state otherwise.
    pub fn observe(&mut self, insn: &DecodedInsn) -> Option<FusionTarget> {
        match insn {
            DecodedInsn::Aarch64(a64) => self.observe_aarch64(a64),
            DecodedInsn::Arm(_) | DecodedInsn::Thumb(_) => self.observe_armv7(insn),
        }
    }

    fn observe_aarch64(&mut self, insn: &Aarch64Insn) -> Option<FusionTarget> {
        use armv8_encode::isa::aarch64::{Aarch64Mnemonic, DecodedOperand, RegisterClass};
        // Collect X-register operand indices in order.
        let mut x_regs: Vec<u8> = Vec::with_capacity(insn.operands.len());
        for op in &insn.operands {
            if let DecodedOperand::Register(r) = op {
                if matches!(r.class, RegisterClass::X | RegisterClass::XOrSp) {
                    x_regs.push(r.index);
                }
            }
        }
        // 1. ADRP — update page slot, no completion.
        if insn.mnemonic == Aarch64Mnemonic::Adrp {
            let page = insn.operands.iter().find_map(|op| match op {
                DecodedOperand::PageTarget(a) => Some(*a),
                _ => None,
            });
            if let (Some(&d), Some(page)) = (x_regs.first(), page) {
                if (d as usize) < self.aarch64_pages.len() {
                    self.aarch64_pages[d as usize] = Some(page);
                }
            }
            return None;
        }
        // 2. ADD Xd, Xs, #imm — potential completion.
        if insn.mnemonic == Aarch64Mnemonic::Add && x_regs.len() >= 2 {
            let d = x_regs[0];
            let s = x_regs[1];
            if let Some(base) = self.aarch64_pages.get(s as usize).copied().flatten() {
                // Pull the first immediate.
                let imm = insn.operands.iter().find_map(|op| match op {
                    DecodedOperand::Immediate(v) => Some(*v),
                    DecodedOperand::UnsignedImmediate(v) => Some(*v as i64),
                    DecodedOperand::ShiftedImmediate(s) => {
                        Some(s.value.wrapping_shl(s.shift as u32))
                    }
                    _ => None,
                });
                if let Some(imm) = imm {
                    if imm >= 0 {
                        // Completion. Per legacy semantics, the
                        // destination register is NOT invalidated
                        // here — callers used to fall through to a
                        // `dest_x_reg` invalidate only in the
                        // non-ADRP, non-completing-ADD branch.
                        // However in practice the listing always
                        // overwrote `x_page_bases[d]` to None when
                        // d != s via the dest_x_reg path. To match
                        // the legacy behaviour, leave it as-is and
                        // let the next non-completing write clear
                        // it.
                        let _ = d;
                        return Some(FusionTarget {
                            target: base.wrapping_add(imm as u64),
                            source_register: s,
                        });
                    }
                }
            }
        }
        // 3. Any other write to an X register invalidates that slot.
        if let Some(&d) = x_regs.first() {
            if (d as usize) < self.aarch64_pages.len() {
                self.aarch64_pages[d as usize] = None;
            }
        }
        None
    }

    fn observe_armv7(&mut self, insn: &DecodedInsn) -> Option<FusionTarget> {
        // movw Rd, #lo16 — store the low half.
        if let Some((rd, lo)) = insn.armv7_movw() {
            if (rd as usize) < self.armv7_movw_lo.len() {
                self.armv7_movw_lo[rd as usize] = Some(lo);
            }
            return None;
        }
        // movt Rd, #hi16 — complete the pair if a low half was
        // pending for this register.
        if let Some((rd, hi)) = insn.armv7_movt() {
            if (rd as usize) < self.armv7_movw_lo.len() {
                if let Some(lo) = self.armv7_movw_lo[rd as usize].take() {
                    let fused = (u32::from(hi) << 16) | u32::from(lo);
                    return Some(FusionTarget {
                        target: fused as u64,
                        source_register: rd,
                    });
                }
            }
            return None;
        }
        // Any other write to an ARM GPR invalidates the slot.
        if let Some(dest) = insn.dest_register() {
            if dest.kind == RegKind::ArmGpr
                && (dest.index as usize) < self.armv7_movw_lo.len()
            {
                self.armv7_movw_lo[dest.index as usize] = None;
            }
        }
        None
    }
}

impl From<Aarch64Insn> for DecodedInsn {
    fn from(i: Aarch64Insn) -> Self { DecodedInsn::Aarch64(i) }
}
impl From<ArmDecodedInstruction> for DecodedInsn {
    fn from(i: ArmDecodedInstruction) -> Self { DecodedInsn::Arm(i) }
}
impl From<ThumbDecodedInstruction> for DecodedInsn {
    fn from(i: ThumbDecodedInstruction) -> Self { DecodedInsn::Thumb(i) }
}
