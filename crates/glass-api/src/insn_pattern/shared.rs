//! Bits the AArch64 and ARMv7 compilers both rely on.
//!
//! Anything ISA-agnostic — pattern slicing, the bracket-aware
//! comma splitter, the immediate parser, the symbol-name
//! heuristic, the per-call options struct — lives here. The
//! per-ISA `WildcardKind` / `OperandToken` enums stay in their
//! own files because each ISA's encoder takes a different set of
//! operand types and unifying them fought the existing design
//! more than it helped.

use crate::bin_search::Atom;

/// Convenience alias for the symbol-lookup closure threaded
/// through both compilers. Borrowed because callers (the GUI
/// editor) already own the closure on the stack and we don't
/// want to force them to box it.
pub type SymbolLookup<'a> = &'a dyn Fn(&str) -> Option<u64>;

/// Per-call options threaded through the compiler. The defaults
/// match the historical search-only path: address 0, no symbol
/// resolver. The GUI's per-line edit path overrides both so
/// PC-relative encodings come out correct and bare identifiers
/// resolve to absolute addresses.
#[derive(Default)]
pub struct CompileOptions<'a> {
    /// Address the *first* compiled instruction is being placed
    /// at. Drives the encoder's PC-relative delta calculation.
    /// Subsequent instructions in a `;`-separated sequence get
    /// `address + step * index` where `step` depends on the ISA.
    /// Defaults to 0.
    pub address: u64,
    /// Optional symbol resolver. When set, an unrecognised
    /// identifier in operand position is looked up via this
    /// closure and treated as the absolute address it returns.
    /// When `None`, identifiers fail to parse.
    pub symbol_lookup: Option<SymbolLookup<'a>>,
}

/// Split a multi-instruction pattern on `;`, trim, and drop
/// empty pieces. Returns `(index, trimmed_str)` pairs so callers
/// keep the original instruction number for error context.
pub(crate) fn split_instructions(pattern: &str) -> impl Iterator<Item = (usize, &str)> {
    pattern
        .split(';')
        .enumerate()
        .map(|(i, raw)| (i, raw.trim()))
        .filter(|(_, s)| !s.is_empty())
}

/// Split an operand list into comma-separated tokens, respecting
/// nested brackets so commas inside `[…]`, `{…}`, or `<…>` are
/// preserved. Both compilers use the same shape; the difference
/// between AArch64 and ARMv7 is what counts as an "opening
/// bracket". `open_chars` / `close_chars` parameterise that.
pub(crate) fn tokenize_operand_strings(
    s: &str,
    open_chars: &[char],
    close_chars: &[char],
) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in s.chars() {
        if open_chars.contains(&ch) {
            depth += 1;
            cur.push(ch);
        } else if close_chars.contains(&ch) {
            depth -= 1;
            cur.push(ch);
        } else if ch == ',' && depth == 0 {
            parts.push(std::mem::take(&mut cur).trim().to_string());
        } else {
            cur.push(ch);
        }
    }
    let tail = cur.trim().to_string();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

/// Parse a (possibly `#`-prefixed, possibly hex, possibly signed)
/// integer literal. Returns `None` if the string isn't a numeric
/// literal; callers fall through to the next operand kind.
pub(crate) fn try_parse_immediate(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches('#').trim();
    let (sign, body) = if let Some(rest) = s.strip_prefix('-') {
        (-1i64, rest)
    } else if let Some(rest) = s.strip_prefix('+') {
        (1, rest)
    } else {
        (1, s)
    };
    let body = body.trim();
    let parsed = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()?
    } else {
        body.parse::<i64>().ok()?
    };
    Some(sign * parsed)
}

/// Heuristic for "this looks like a symbol name, not a typo":
/// starts with a letter / underscore, body chars limited to the
/// alphabet of typical symbol names (alphanumeric, `_`, `:`,
/// `$`, `.`, `@` — wide enough for mangled C++ / Rust / Swift,
/// DEX, and Obj-C selectors). Stricter than `is_alphanumeric`
/// so a random number-only typo doesn't pretend to be a symbol.
pub(crate) fn looks_like_symbol(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return false };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | ':' | '$' | '.' | '@')
    })
}

/// Render compiled atoms as a human-readable hex string. Bytes
/// with a full 0xff mask render as `xx`; partial-mask bytes
/// render as `xx/MM` with the mask byte after a slash; fully-
/// wildcarded bytes render as `??`.
pub(crate) fn atoms_to_hex(atoms: &[Atom]) -> String {
    atoms
        .iter()
        .map(|a| match a {
            Atom::Mask { mask: 0xff, value } => format!("{value:02x}"),
            Atom::Mask { mask: 0, .. } => "??".to_string(),
            Atom::Mask { mask, value } => format!("{value:02x}/{mask:02x}"),
            Atom::Gap { .. } => "*".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}
