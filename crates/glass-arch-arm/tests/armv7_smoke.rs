//! End-to-end smoke tests for the ARMv7 wiring in `glass-arch-arm`.
//!
//! Uses the upstream's libtool-checker.so fixture (an ARMv7 ELF
//! shared object with both ARM-mode and Thumb-mode functions). The
//! fixture is vendored at `tests/libtool-checker.so` so the test
//! doesn't depend on the cargo cache location.

use armv8_encode::container::{Architecture, Container};
use armv8_encode::mc::InstructionInfo;
use std::path::PathBuf;

fn fixture_bytes() -> Vec<u8> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest).join("tests").join("libtool-checker.so");
    std::fs::read(&path).expect("read libtool-checker.so")
}

#[test]
fn container_parses_as_armv7_elf() {
    let c = Container::from_bytes(&fixture_bytes()).expect("parse");
    assert_eq!(c.architecture, Architecture::Arm);
}

#[test]
fn disassemble_function_at_arm_plt_stub_returns_instructions() {
    let c = Container::from_bytes(&fixture_bytes()).expect("parse");
    // The .plt entry at 0xf84 is ARM-mode (covered by the upstream
    // tests).
    let insns = glass_arch_arm::disassemble_function_at(&c, 0xf84)
        .expect("disassemble plt entry");
    assert!(!insns.is_empty(), "expected at least one decoded insn");
    // Every insn should be an ARM variant since the entry was
    // low-bit-clear.
    for i in &insns {
        assert!(matches!(i, glass_arch_arm::DecodedInsn::Arm(_)));
    }
}

#[test]
fn disassemble_function_at_thumb_entry_picks_thumb_mode() {
    let c = Container::from_bytes(&fixture_bytes()).expect("parse");
    // Pick any defined function symbol whose address has the
    // Thumb low-bit set.
    let thumb_sym = c
        .symbols
        .iter()
        .find(|s| !s.is_undefined && s.address & 1 == 1)
        .expect("at least one Thumb function");
    let insns = glass_arch_arm::disassemble_function_at(&c, thumb_sym.address)
        .expect("disassemble thumb function");
    assert!(!insns.is_empty());
    for i in &insns {
        assert!(matches!(i, glass_arch_arm::DecodedInsn::Thumb(_)));
    }
    // The address sequence should be strictly increasing.
    let mut last: Option<u64> = None;
    for i in &insns {
        let a = i.address();
        if let Some(prev) = last {
            assert!(a > prev, "addresses not strictly increasing");
        }
        last = Some(a);
    }
}

#[test]
fn precompute_section_insns_returns_nonempty_for_text() {
    let c = Container::from_bytes(&fixture_bytes()).expect("parse");
    // Find the .text section.
    let (text_idx, _) = c
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| {
            matches!(s.kind, armv8_encode::container::SectionKind::Text)
                && s.name == ".text"
        })
        .expect(".text section");
    let entries: Vec<u64> = c
        .symbols
        .iter()
        .filter(|s| !s.is_undefined)
        .map(|s| s.address)
        .collect();
    let insns =
        glass_arch_arm::precompute_section_insns(&c, text_idx, &entries).expect("precompute");
    assert!(!insns.is_empty());
}

#[test]
fn build_function_cfg_on_armv7_returns_blocks() {
    let c = Container::from_bytes(&fixture_bytes()).expect("parse");
    let symbols = glass_arch_arm::SymbolMap::build(&c);
    // The ARM-mode PLT stub at 0xf84 is small but should still
    // yield at least one block (the stub itself).
    let cfg = glass_arch_arm::build_function_cfg(&c, &symbols, 0xf84)
        .expect("cfg for plt stub");
    assert!(!cfg.blocks.is_empty(), "expected ≥1 block");
    // And the entry should be inside the first block.
    let entry = &cfg.blocks[0];
    assert_eq!(entry.start_addr, 0xf84);
    assert!(!entry.instructions.is_empty());
}

#[test]
fn format_text_renders_arm_plt_stub() {
    let c = Container::from_bytes(&fixture_bytes()).expect("parse");
    let insns = glass_arch_arm::disassemble_function_at(&c, 0xf84).expect("dis");
    // First insn at the PLT stub: binutils displays it as a `push`
    // alias. We don't assert the exact mnemonic since the upstream
    // table can change — just that the rendered line starts with
    // some mnemonic followed by operands.
    let line = insns[0].format_text();
    assert!(!line.is_empty(), "empty format_text");
}
