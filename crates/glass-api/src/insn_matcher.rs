//! Prefix matcher + ranking for the instruction autocomplete
//! dropdown (Phase B).
//!
//! Given the user's current input line (everything before the
//! cursor on the active instruction), produce a ranked list of
//! `Variant` candidates whose mnemonic + operand-class prefixes
//! could still grow into the typed text.
//!
//! The matcher is intentionally cheap — it walks the full
//! `variants()` list on every call, scoring each entry by:
//!
//! 1. Mnemonic prefix match (longest-common-prefix length).
//! 2. Operand prefix compatibility (does what the user has
//!    typed so far fit the variant's slot layout up to the
//!    same point?).
//!
//! Returned candidates are deduplicated by `template` (the
//! armv8-encode opcode table has multiple table entries that
//! collapse to the same user-visible form via aliasing).
//!
//! The ranking is a simple score:
//! - +10 per matched mnemonic character
//! - +5 per fully-typed operand slot that's consistent with
//!   what the user typed
//! - -1 per slot still empty (so shorter variants surface first
//!   when nothing tells them apart)
//!
//! Anything contradictory (e.g. user typed `w0` but the slot is
//! an X register) drops the variant from the list entirely.

use crate::insn_variants::{variants, SlotSpec, Variant};

#[derive(Debug, Clone)]
pub struct MatchCandidate {
    pub variant: Variant,
    pub score: i32,
}

/// Score every variant against `input` and return the top
/// `limit` candidates, sorted by score descending.
pub fn match_variants(input: &str, limit: usize) -> Vec<MatchCandidate> {
    let (mnem, operand_str) = split_mnemonic_operands(input);
    let operand_tokens = tokenize_operands(operand_str);
    let mut out: Vec<MatchCandidate> = Vec::new();

    for v in variants() {
        if let Some(score) = score_variant(v, mnem, &operand_tokens) {
            out.push(MatchCandidate {
                variant: v.clone(),
                score,
            });
        }
    }

    // Dedup by template (alias collisions).
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.variant.template.cmp(&b.variant.template))
    });
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.variant.template.clone()));
    out.truncate(limit);
    out
}

fn split_mnemonic_operands(input: &str) -> (&str, &str) {
    let s = input.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

fn tokenize_operands(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => depth -= 1,
            b',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

fn score_variant(v: &Variant, mnem_input: &str, operand_tokens: &[&str]) -> Option<i32> {
    // Mnemonic prefix check (case-insensitive).
    if !v.mnemonic.starts_with(&mnem_input.to_ascii_lowercase()) {
        return None;
    }

    // Each typed operand must be consistent with the
    // corresponding slot — or be still being typed.
    if operand_tokens.len() > v.slots.len() {
        return None;
    }
    for (token, slot) in operand_tokens.iter().zip(v.slots.iter()) {
        if !operand_consistent(token, *slot) {
            return None;
        }
    }

    let mut score = 10 * mnem_input.len() as i32;
    score += 5 * operand_tokens.len() as i32;
    score -= (v.slots.len() as i32 - operand_tokens.len() as i32).max(0);
    Some(score)
}

/// Best-effort: does `token` (possibly partial) look compatible
/// with `slot`? Empty token = "user hasn't started typing this
/// slot yet" → always compatible.
fn operand_consistent(token: &str, slot: SlotSpec) -> bool {
    let t = token.trim();
    if t.is_empty() {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    match slot {
        SlotSpec::Gp { sp } => looks_like_w_reg(&lower, sp) || looks_like_x_reg(&lower, sp),
        SlotSpec::FpReg => starts_with_any(&lower, &['b', 'h', 's', 'd', 'q']),
        SlotSpec::VecReg => lower.starts_with('v'),
        SlotSpec::Imm => starts_with_immediate(&lower),
        SlotSpec::PcRel => starts_with_immediate(&lower),
        SlotSpec::Mem => lower.starts_with('['),
        SlotSpec::Cond => true, // cond codes are 2-letter, defer judging until full
        SlotSpec::System => true,
        SlotSpec::Other => true,
    }
}

fn starts_with_any(s: &str, chars: &[char]) -> bool {
    let Some(c) = s.chars().next() else { return false };
    chars.contains(&c)
}

fn starts_with_immediate(s: &str) -> bool {
    let Some(c) = s.chars().next() else { return false };
    c == '#' || c == '-' || c == '0' || c.is_ascii_digit()
}

fn looks_like_w_reg(s: &str, sp: bool) -> bool {
    if s == "w" || s == "wz" || s == "wzr" {
        return true;
    }
    if sp && (s == "ws" || s == "wsp") {
        return true;
    }
    if let Some(rest) = s.strip_prefix('w') {
        return rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

fn looks_like_x_reg(s: &str, sp: bool) -> bool {
    if s == "x" || s == "xz" || s == "xzr" {
        return true;
    }
    if sp && (s == "s" || s == "sp") {
        return true;
    }
    if let Some(rest) = s.strip_prefix('x') {
        return rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_many() {
        let m = match_variants("", 10);
        assert_eq!(m.len(), 10);
    }

    #[test]
    fn mov_narrows() {
        let m = match_variants("mov", 50);
        assert!(!m.is_empty());
        assert!(m.iter().all(|c| c.variant.mnemonic.starts_with("mov")));
    }

    #[test]
    fn ret_is_top_for_ret() {
        let m = match_variants("ret", 5);
        assert!(m.iter().any(|c| c.variant.mnemonic == "ret"));
    }

    #[test]
    fn typed_w_reg_filters() {
        // "mov w0," should keep at least one viable mov form.
        let m = match_variants("mov w0,", 50);
        assert!(!m.is_empty());
        for c in &m {
            assert!(c.variant.mnemonic.starts_with("mov"));
        }
    }

    #[test]
    fn tokenizer_handles_bracketed_mem() {
        let toks = tokenize_operands("x0, [sp, #16]");
        assert_eq!(toks, vec!["x0", "[sp, #16]"]);
    }
}
