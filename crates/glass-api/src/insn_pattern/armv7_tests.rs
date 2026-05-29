//! Tests extracted from `armv7.rs` so the impl side stays under
//! the per-file size cap. Imported via the parent module so all
//! the items it pokes at remain crate-private.

#![cfg(test)]

use super::armv7::{compile_armv7_at, compile_armv7_to_atoms};
use crate::bin_search::Atom;

fn atoms_to_word_thumb16(atoms: &[Atom]) -> (u16, u16) {
    assert_eq!(atoms.len(), 2);
    let (mb0, vb0) = match atoms[0] {
        Atom::Mask { mask, value } => (mask, value),
        _ => panic!("not a mask atom"),
    };
    let (mb1, vb1) = match atoms[1] {
        Atom::Mask { mask, value } => (mask, value),
        _ => panic!("not a mask atom"),
    };
    let word = u16::from_le_bytes([vb0, vb1]);
    let mask = u16::from_le_bytes([mb0, mb1]);
    (word, mask)
}

#[test]
fn parse_mov_r1_r7_thumb_concrete() {
    // `mov r1, r7` -> 16-bit Thumb encoding 0x4639.
    let atoms = compile_armv7_to_atoms("mov r1, r7").expect("compile");
    let (word, mask) = atoms_to_word_thumb16(&atoms);
    assert_eq!(mask, 0xffff, "concrete operands -> fully-fixed mask");
    assert_eq!(word, 0x4639, "expected mov r1, r7 = 0x4639, got 0x{word:04x}");
}

#[test]
fn conditional_thumb_branch() {
    // `beq` with wildcard target -> 16-bit T1 conditional B
    // with opcode bits 11..15 = 0b1101, cond=0 in bits 8..11.
    let atoms = compile_armv7_to_atoms("beq <*>").expect("compile beq");
    let (word, mask) = atoms_to_word_thumb16(&atoms);
    assert_eq!(word >> 12, 0xd, "expected B-cond top nibble");
    assert_eq!((word >> 8) & 0xf, 0x0, "expected eq cond");
    assert_eq!(mask & 0xff, 0x00, "branch target bits should be masked");
}

#[test]
fn push_register_list_thumb() {
    // `push {r4, r5, lr}` -> 0xb530.
    let atoms = compile_armv7_to_atoms("push {r4, r5, lr}").expect("compile push");
    let (word, mask) = atoms_to_word_thumb16(&atoms);
    assert_eq!(mask, 0xffff, "concrete list -> fully fixed");
    assert_eq!(word, 0xb530, "got 0x{word:04x}");
}

#[test]
fn ldr_simple_memory_thumb() {
    let atoms = compile_armv7_to_atoms("ldr r3, [r4]").expect("compile ldr");
    assert!(matches!(atoms.len(), 2 | 4), "got {} atoms", atoms.len());
}

#[test]
fn wildcard_register_thumb() {
    let atoms = compile_armv7_to_atoms("mov r1, <R>").expect("compile");
    assert!(atoms.len() >= 2);
    let any_partial = atoms
        .iter()
        .any(|a| matches!(a, Atom::Mask { mask, .. } if *mask != 0xff));
    assert!(any_partial, "wildcard should produce partial-mask byte");
}

#[test]
fn rejects_aarch64_register_names() {
    let err = compile_armv7_to_atoms("mov w0, #1").expect_err("should reject");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("AArch64") || msg.contains("w0") || msg.contains("ARM"),
        "msg: {msg}"
    );
}

// ---- compile_armv7_at editor smoke ------------------------

#[test]
fn compile_armv7_at_thumb_16bit_returns_two_bytes() {
    let bytes = compile_armv7_at("nop", 0x1000, true, None).expect("compile thumb nop");
    assert_eq!(bytes, vec![0x00, 0xbf]);
}

#[test]
fn compile_armv7_at_thumb_32bit_returns_four_bytes() {
    let bytes =
        compile_armv7_at("movw r3, #0x1234", 0x1000, true, None).expect("compile movw");
    assert_eq!(bytes.len(), 4);
}

#[test]
fn compile_armv7_at_arm_mode_returns_four_bytes() {
    let bytes = compile_armv7_at("bx lr", 0x1000, false, None).expect("compile bx lr");
    assert_eq!(bytes.len(), 4);
}

#[test]
fn compile_armv7_at_rejects_multi_instruction() {
    let err = compile_armv7_at("nop; nop", 0x1000, true, None)
        .expect_err("should reject multi-instruction");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("multi") || msg.contains("one instruction"),
        "msg: {msg}"
    );
}

#[test]
fn bl_resolves_symbol_in_thumb_mode() {
    let lookup = |name: &str| -> Option<u64> {
        (name == "pthread_mutex_init").then_some(0x2000)
    };
    let bytes = compile_armv7_at(
        "bl pthread_mutex_init",
        0x1000,
        true,
        Some(&lookup),
    )
    .expect("compile bl with symbol");
    assert_eq!(bytes.len(), 4, "Thumb-2 BL is 4 bytes");
}

#[test]
fn bl_resolves_symbol_in_arm_mode() {
    let lookup = |name: &str| -> Option<u64> {
        (name == "_start").then_some(0x8004)
    };
    let bytes = compile_armv7_at("bl _start", 0x8000, false, Some(&lookup))
        .expect("compile arm bl");
    assert_eq!(bytes.len(), 4);
}

#[test]
fn unknown_symbol_errors() {
    let lookup = |_: &str| -> Option<u64> { None };
    let err = compile_armv7_at("bl mystery_func", 0x1000, true, Some(&lookup))
        .expect_err("should fail to resolve");
    assert!(format!("{err:#}").contains("mystery_func"));
}

#[test]
fn symbol_with_no_resolver_errors() {
    let err = compile_armv7_at("bl decode_packet", 0x1000, true, None)
        .expect_err("no resolver");
    assert!(
        format!("{err:#}").contains("resolver")
            || format!("{err:#}").contains("symbol"),
        "msg: {err:#}"
    );
}

#[test]
fn arm_mode_bx_lr_conditional() {
    let atoms = compile_armv7_to_atoms("bxeq lr").expect("compile bxeq lr");
    assert_eq!(atoms.len(), 4, "ARM mode = 4 bytes");
    let bytes: Vec<u8> = atoms
        .iter()
        .map(|a| match a {
            Atom::Mask { value, .. } => *value,
            _ => panic!(),
        })
        .collect();
    let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let masks: Vec<u8> = atoms
        .iter()
        .map(|a| match a {
            Atom::Mask { mask, .. } => *mask,
            _ => panic!(),
        })
        .collect();
    let mask = u32::from_le_bytes([masks[0], masks[1], masks[2], masks[3]]);
    assert_eq!(mask, 0xffff_ffff);
    assert_eq!(word, 0x012f_ff1e, "got 0x{word:08x}");
}
