//! ARMv7 binutils-format-string operand-slot classifier.
//!
//! Extracted from `armv7.rs` to keep that module under the 1000-
//! line workspace discipline. The classifier reads a row's format
//! string (e.g. `"mov%c\t%D, %0-3r"`) and returns the ordered list
//! of operand slot kinds the encoder will consume — the parser
//! uses this to know how many operand tokens to expect and how to
//! shape each one.

use super::armv7::Mode;

#[derive(Debug, Clone, Copy)]
pub(super) enum SlotKind {
    Register,
    Immediate,
    BranchTarget,
    RegisterList,
    Memory,
    Condition,
    Opaque,
    Other,
}

/// Parse a binutils format string into a list of operand-slot
/// kinds. This mirrors what `decode_operands_from_format` and the
/// encoder's pack loop consume, so the order matches the
/// encoder's expectations.
///
/// Mode-sensitive: bare `%c` (no bitfield) is display-only in
/// Thumb but consumes a Condition in ARM mode.
pub(super) fn format_slot_kinds(format: &str, mode: Mode) -> Vec<SlotKind> {
    let mut out = Vec::new();
    let bytes = format.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            break;
        }
        // {X:...%} display wrappers — recurse over inner.
        if bytes[i] == b'{' {
            let inner_start = i + 1;
            let mut j = inner_start;
            while j + 1 < bytes.len() {
                if bytes[j] == b'%' && bytes[j + 1] == b'}' {
                    break;
                }
                j += 1;
            }
            let inner = if inner_start + 2 <= j {
                std::str::from_utf8(&bytes[inner_start + 2..j]).unwrap_or("")
            } else {
                ""
            };
            out.extend(format_slot_kinds(inner, mode));
            i = j + 2;
            continue;
        }
        let bf_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'-') {
            i += 1;
        }
        let bf = std::str::from_utf8(&bytes[bf_start..i]).unwrap_or("");
        if i >= bytes.len() {
            break;
        }
        let code = bytes[i];
        i += 1;
        match code {
            b'\'' => {
                if i < bytes.len() {
                    i += 1;
                }
                continue;
            }
            b'?' => {
                if i + 1 < bytes.len() {
                    i += 2;
                }
                continue;
            }
            b'`' => {
                if i < bytes.len() {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        // Bitfielded condition (%8-11c) consumes a Condition in
        // both modes.
        if code == b'c' && !bf.is_empty() {
            out.push(SlotKind::Condition);
            continue;
        }
        // Bare %c is display-only in Thumb, but consumes a
        // Condition operand in ARM mode (the ARM-mode encoder
        // packs it into bits 28..31).
        if code == b'c' && bf.is_empty() {
            if mode == Mode::Arm {
                out.push(SlotKind::Condition);
            }
            continue;
        }
        // Display-only.
        if matches!(code, b'C' | b'x' | b'X' | b'%' | b'p' | b't' | b'q') && bf.is_empty() {
            continue;
        }
        if matches!(code, b'w' | b'W') && bf.is_empty() {
            continue;
        }
        match code {
            b'r' | b'R' | b'T' | b'S' | b'D' => out.push(SlotKind::Register),
            b'd' | b'W' | b'H' | b'x' | b'X' | b'I' | b'J' | b'V' | b'e' | b'E' | b'U' => {
                out.push(SlotKind::Immediate)
            }
            b'B' | b'b' => out.push(SlotKind::BranchTarget),
            b'a' if !bf.is_empty() => out.push(SlotKind::BranchTarget),
            b'M' | b'N' | b'O' => out.push(SlotKind::RegisterList),
            b'a' | b's' | b'o' => out.push(SlotKind::Memory),
            b'L' | b'F' | b'm' | b'n' => out.push(SlotKind::Opaque),
            _ => out.push(SlotKind::Other),
        }
    }
    out
}
