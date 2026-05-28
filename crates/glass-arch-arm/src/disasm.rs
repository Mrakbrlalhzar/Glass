//! Architecture-aware disassembly entry points.
//!
//! Routes by the container's architecture: AArch64 uses the existing
//! linear sweep over fixed 4-byte chunks; ARMv7 uses the upstream
//! recursive-descent disassembler so literal pools and the ARM/Thumb
//! mode split are honored.

use armv8_encode::container::{Architecture, Container, SectionKind};
use armv8_encode::isa::aarch64;
use armv8_encode::isa::armv7;

use crate::facade::DecodedInsn;

/// Disassemble the function at `entry_addr` and return its
/// instructions in source order. Returns `None` when the address
/// isn't inside any text section.
///
/// AArch64 path: linear sweep from `entry_addr` to either the
/// covering symbol's end or the section end (whichever is smaller).
/// ARMv7 path: upstream's recursive-descent disassembler, picking
/// ARM vs Thumb mode from the low bit of the entry address.
pub fn disassemble_function_at(
    container: &Container,
    entry_addr: u64,
) -> Option<Vec<DecodedInsn>> {
    match container.architecture {
        Architecture::Aarch64 => disassemble_function_aarch64(container, entry_addr),
        Architecture::Arm => disassemble_function_arm(container, entry_addr),
        Architecture::Other => None,
    }
}

fn disassemble_function_aarch64(
    container: &Container,
    entry_addr: u64,
) -> Option<Vec<DecodedInsn>> {
    let section = container.sections.iter().find(|s| {
        matches!(s.kind, SectionKind::Text)
            && entry_addr >= s.address
            && entry_addr < s.address + s.size
    })?;
    // Same heuristic the CFG uses: walk to the section end.
    // Callers that have a SymbolMap can clamp further by checking
    // sym.size — but for the bare entry-point case we sweep until
    // we hit a return-shaped terminator (best-effort).
    let off = (entry_addr - section.address) as usize;
    let mut out = Vec::new();
    let mut cursor = off;
    while cursor + 4 <= section.bytes.len() {
        let addr = section.address + cursor as u64;
        let word = u32::from_le_bytes([
            section.bytes[cursor],
            section.bytes[cursor + 1],
            section.bytes[cursor + 2],
            section.bytes[cursor + 3],
        ]);
        match aarch64::decode_instruction(addr, word) {
            Ok(insn) => {
                let mnem = insn.mnemonic;
                out.push(DecodedInsn::Aarch64(insn));
                if crate::format::is_terminator(mnem) && !crate::format::is_call(mnem) {
                    // Stop at the first non-call terminator (ret,
                    // unconditional branch, brk, …). Conditional
                    // branches are also terminators per `is_terminator`
                    // but we keep walking past them — the section may
                    // have another block immediately after.
                    if matches!(
                        mnem,
                        armv8_encode::isa::aarch64::Aarch64Mnemonic::Ret
                            | armv8_encode::isa::aarch64::Aarch64Mnemonic::Eret
                            | armv8_encode::isa::aarch64::Aarch64Mnemonic::B
                    ) {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
        cursor += 4;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn disassemble_function_arm(
    container: &Container,
    entry_addr: u64,
) -> Option<Vec<DecodedInsn>> {
    let fd = armv7::disassemble_function_at(container, entry_addr)?;
    let mut out: Vec<DecodedInsn> = Vec::new();
    match fd {
        armv7::FunctionDisassembly::Arm(a) => {
            for i in a.instructions {
                out.push(DecodedInsn::Arm(i));
            }
        }
        armv7::FunctionDisassembly::Thumb(t) => {
            for i in t.instructions {
                out.push(DecodedInsn::Thumb(i));
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Disassemble every reachable instruction in `section`, given a
/// list of function entry addresses. Used by the loader to
/// pre-compute the per-section `Vec<DecodedInsn>` that the ARMv7
/// listing renders from. For AArch64 the loader currently
/// continues to decode on demand, so this returns `None` for
/// AArch64 to keep the AArch64 path byte-identical with today.
pub fn precompute_section_insns(
    container: &Container,
    section_index: usize,
    entry_points: &[u64],
) -> Option<Vec<DecodedInsn>> {
    let section = container.sections.get(section_index)?;
    if !matches!(section.kind, SectionKind::Text) {
        return None;
    }
    match container.architecture {
        Architecture::Arm => precompute_section_arm(section, entry_points),
        _ => None,
    }
}

fn precompute_section_arm(
    section: &armv8_encode::container::Section,
    entry_points: &[u64],
) -> Option<Vec<DecodedInsn>> {
    use armv7::arm::recursive::disassemble_recursive as arm_recursive;
    use armv7::recursive::disassemble_recursive as thumb_recursive;

    // Partition entry points by mode (low-bit set = Thumb).
    let base = section.address;
    let end = base + section.size;
    let in_section = |a: u64| {
        let real = a & !1u64;
        real >= base && real < end
    };
    let arm_entries: Vec<u64> = entry_points
        .iter()
        .copied()
        .filter(|&a| a & 1 == 0 && in_section(a))
        .collect();
    let thumb_entries: Vec<u64> = entry_points
        .iter()
        .copied()
        .filter(|&a| a & 1 == 1 && in_section(a))
        .collect();

    let bytes = &section.bytes;
    let mut by_addr: std::collections::BTreeMap<u64, DecodedInsn> =
        std::collections::BTreeMap::new();

    if !arm_entries.is_empty() {
        let dis = arm_recursive(base, bytes, &arm_entries);
        for i in dis.instructions {
            by_addr.insert(i.address, DecodedInsn::Arm(i));
        }
    }
    if !thumb_entries.is_empty() {
        let dis = thumb_recursive(base, bytes, &thumb_entries);
        for i in dis.instructions {
            by_addr.insert(i.address, DecodedInsn::Thumb(i));
        }
    }
    Some(by_addr.into_values().collect())
}
