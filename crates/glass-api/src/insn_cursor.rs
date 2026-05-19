//! Cursor-position classifier for the instruction edit field.
//!
//! Given the raw input string and a byte cursor position, work
//! out what kind of token the cursor is sitting in. The disasm
//! editor uses this to pick which suggestion source to show:
//!
//! - `Mnemonic` → variants index (mnemonic templates)
//! - `RegisterSlot` → register-name list (W0..W30, X0..X30, …)
//! - `BranchTargetSlot` → symbol map (PC-relative slots accept
//!   symbol names or hex addresses)
//! - `ImmediateSlot` → no suggestions (raw numeric input)
//! - `MemorySlot` → no suggestions (need to land elsewhere when
//!   we add `[reg, #offset]` autocomplete)
//!
//! The classifier is approximate — it doesn't look at the
//! opcode table to know whether slot 1 of `bl` is PC-relative;
//! instead it uses a small per-mnemonic lookup of mnemonics
//! that take a branch target. Everything else falls back to a
//! generic operand-slot category derived from punctuation.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorKind {
    /// Cursor is in the mnemonic word (or before any whitespace).
    Mnemonic,
    /// Cursor is in operand slot `index` (0-based), and that
    /// slot is a branch / PC-relative target — accepts symbol
    /// names.
    BranchTargetSlot,
    /// Generic operand slot — register or unknown. Register
    /// suggestions are useful here.
    RegisterSlot,
    /// Cursor is inside an immediate token (started with `#` or
    /// a digit). No suggestions.
    ImmediateSlot,
    /// Cursor is inside `[...]`. No suggestions in v1.
    MemorySlot,
}

#[derive(Debug, Clone)]
pub struct CursorContext {
    pub kind: CursorKind,
    /// Byte range in the input that holds the partial word the
    /// cursor is in. Used by the dropdown's commit step to
    /// replace just this range with the chosen suggestion.
    pub word_range: Range<usize>,
    /// The partial word's text, lower-cased for prefix matching.
    pub partial: String,
}

/// Mnemonics whose first operand is a branch / PC-relative
/// target. Anything in this set drives `BranchTargetSlot` once
/// the cursor lands past the mnemonic.
const BRANCH_MNEMONICS: &[&str] = &[
    "b", "bl", "br", "blr", "ret", "cbz", "cbnz", "tbz", "tbnz",
    "adr", "adrp",
];

/// Classify the cursor at `cursor` (byte offset into `input`).
pub fn classify(input: &str, cursor: usize) -> CursorContext {
    let cursor = cursor.min(input.len());
    // Find the word the cursor sits in: walk back to the
    // previous space / comma / `[` / `]` / `(`, walk forward
    // to the next.
    let bytes = input.as_bytes();
    let is_word_boundary = |c: u8| matches!(c, b' ' | b'\t' | b',' | b'[' | b']' | b'(' | b')');
    let mut start = cursor;
    while start > 0 && !is_word_boundary(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = cursor;
    while end < bytes.len() && !is_word_boundary(bytes[end]) {
        end += 1;
    }
    let partial = input[start..end].to_ascii_lowercase();

    // Are we still in the mnemonic? "Still in" = nothing in the
    // input before this word but whitespace.
    let prefix = &input[..start];
    let is_mnemonic = prefix.trim().is_empty();
    if is_mnemonic {
        return CursorContext {
            kind: CursorKind::Mnemonic,
            word_range: start..end,
            partial,
        };
    }

    // Inside `[...]` → memory slot.
    if in_brackets(input, cursor) {
        return CursorContext {
            kind: CursorKind::MemorySlot,
            word_range: start..end,
            partial,
        };
    }

    // Immediate? Starts with `#`, `0x`, `-`, or a digit.
    if looks_immediate(&partial) {
        return CursorContext {
            kind: CursorKind::ImmediateSlot,
            word_range: start..end,
            partial,
        };
    }

    // What's the mnemonic of this instruction?
    let mnem = first_word(input).unwrap_or("").to_ascii_lowercase();
    if BRANCH_MNEMONICS.contains(&mnem.as_str()) {
        // For `adr`/`adrp`/`cbz`/`tbz`/etc., the branch target
        // is the *last* operand. We treat any non-immediate
        // non-register operand here as a branch slot.
        if !looks_like_register(&partial) {
            return CursorContext {
                kind: CursorKind::BranchTargetSlot,
                word_range: start..end,
                partial,
            };
        }
    }
    CursorContext {
        kind: CursorKind::RegisterSlot,
        word_range: start..end,
        partial,
    }
}

fn in_brackets(input: &str, cursor: usize) -> bool {
    let mut depth = 0i32;
    for (i, c) in input.char_indices() {
        if i >= cursor {
            break;
        }
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

fn looks_immediate(s: &str) -> bool {
    let s = s.trim_start_matches('#');
    let s = s.trim_start_matches('-');
    s.starts_with("0x") || s.starts_with("0X") || s.chars().next().map_or(false, |c| c.is_ascii_digit())
}

fn looks_like_register(s: &str) -> bool {
    if matches!(s, "sp" | "wsp" | "xzr" | "wzr") {
        return true;
    }
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    let first = bytes[0] | 0x20;
    if first != b'w' && first != b'x' && first != b'v' && first != b'q' && first != b'd' && first != b's' {
        return false;
    }
    bytes[1..].iter().all(|b| b.is_ascii_digit())
}

fn first_word(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
    if end == 0 {
        None
    } else {
        Some(&s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_mnemonic() {
        let c = classify("", 0);
        assert_eq!(c.kind, CursorKind::Mnemonic);
        assert_eq!(c.word_range, 0..0);
        assert_eq!(c.partial, "");
    }

    #[test]
    fn partial_mnemonic() {
        let c = classify("mo", 2);
        assert_eq!(c.kind, CursorKind::Mnemonic);
        assert_eq!(c.partial, "mo");
        assert_eq!(c.word_range, 0..2);
    }

    #[test]
    fn after_mnemonic_in_register_slot() {
        let c = classify("mov x", 5);
        assert_eq!(c.kind, CursorKind::RegisterSlot);
        assert_eq!(c.partial, "x");
    }

    #[test]
    fn branch_target_after_bl() {
        let c = classify("bl deco", 7);
        assert_eq!(c.kind, CursorKind::BranchTargetSlot);
        assert_eq!(c.partial, "deco");
    }

    #[test]
    fn immediate_with_hash() {
        let c = classify("mov w0, #1", 10);
        assert_eq!(c.kind, CursorKind::ImmediateSlot);
    }

    #[test]
    fn memory_brackets() {
        let c = classify("ldr x0, [sp, #16]", 9);
        assert_eq!(c.kind, CursorKind::MemorySlot);
    }
}
