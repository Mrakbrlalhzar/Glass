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
//!    For ARMv7 variants with `cond_suffix_allowed`, the input
//!    is also tested against `mnemonic + <cond-prefix>` so
//!    typing `bxeq` matches the base-mnemonic `bx` row.
//! 2. Operand prefix compatibility (does what the user has
//!    typed so far fit the variant's slot layout up to the
//!    same point?). ISA-aware: typing `r1` rules out every
//!    AArch64 variant; typing `w0`/`x0` rules out every ARMv7
//!    variant.
//!
//! Returned candidates are deduplicated by `template` (multiple
//! table entries collapse to the same user-visible form via
//! aliasing).
//!
//! The ranking is a simple score:
//! - +10 per matched mnemonic character
//! - +5 per fully-typed operand slot that's consistent with
//!   what the user typed
//! - -1 per slot still empty (so shorter variants surface first
//!   when nothing tells them apart)

use crate::insn_variants::{variants, SlotSpec, Variant};
#[cfg(test)]
use crate::insn_variants::VariantIsa;

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
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth -= 1,
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
    // Mnemonic prefix check (case-insensitive). For variants
    // whose mnemonic admits a conditional suffix, also try
    // matching `mnemonic + cond-prefix`.
    let lc = mnem_input.to_ascii_lowercase();
    if !mnemonic_prefix_matches(v, &lc) {
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

/// Returns true iff `input_lc` (lowercase) is a prefix of (or
/// equal to) the variant's textual mnemonic, OR — for variants
/// with `cond_suffix_allowed` — a prefix of `mnemonic + <known
/// cond suffix prefix>`.
fn mnemonic_prefix_matches(v: &Variant, input_lc: &str) -> bool {
    if v.mnemonic.starts_with(input_lc) {
        return true;
    }
    if !v.cond_suffix_allowed {
        return false;
    }
    let Some(tail) = input_lc.strip_prefix(v.mnemonic) else {
        return false;
    };
    // Empty tail is the base case (covered above); here tail is
    // non-empty. Accept it if it's a 1-or-2 character prefix of
    // any known cond suffix.
    if tail.len() > 2 {
        return false;
    }
    KNOWN_CONDS.iter().any(|c| c.starts_with(tail))
}

const KNOWN_CONDS: &[&str] = &[
    "eq", "ne", "cs", "hs", "cc", "lo", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le",
    "al",
];

/// Best-effort: does `token` (possibly partial) look compatible
/// with `slot`? Empty token = "user hasn't started typing this
/// slot yet" → always compatible.
fn operand_consistent(token: &str, slot: SlotSpec) -> bool {
    let t = token.trim();
    if t.is_empty() {
        return true;
    }
    let lower = t.to_ascii_lowercase();

    // Cross-ISA hard rejections. Typing `r1` in a slot that's
    // a clearly-AArch64 GP/FP register rules out the variant.
    // Typing `w0` / `x0` in an Arm slot does the same.
    if slot.is_aarch64() && looks_like_arm_only_token(&lower) {
        return false;
    }
    if slot.is_armv7() && looks_like_aarch64_only_token(&lower) {
        return false;
    }

    match slot {
        SlotSpec::Gp { sp } => looks_like_w_reg(&lower, sp) || looks_like_x_reg(&lower, sp),
        SlotSpec::FpReg => starts_with_any(&lower, &['b', 'h', 's', 'd', 'q']),
        SlotSpec::VecReg => lower.starts_with('v'),
        SlotSpec::Imm => starts_with_immediate(&lower),
        SlotSpec::PcRel => starts_with_immediate(&lower),
        SlotSpec::Mem => lower.starts_with('['),
        SlotSpec::Cond => true,
        SlotSpec::System => true,
        SlotSpec::Other => true,

        SlotSpec::ArmGp { sp } => looks_like_arm_reg(&lower, sp),
        SlotSpec::ArmImm => starts_with_immediate(&lower),
        SlotSpec::ArmMem => lower.starts_with('['),
        SlotSpec::ArmRegList => lower.starts_with('{'),
        SlotSpec::ArmCond => true,
        // Shifted operand starts with a register.
        SlotSpec::ArmShifted => looks_like_arm_reg(&lower, true),
        SlotSpec::ArmBranch => starts_with_immediate(&lower) || lower.starts_with("0x"),
    }
}

fn starts_with_any(s: &str, chars: &[char]) -> bool {
    let Some(c) = s.chars().next() else { return false };
    chars.contains(&c)
}

fn starts_with_immediate(s: &str) -> bool {
    let Some(c) = s.chars().next() else { return false };
    c == '#' || c == '-' || c == '+' || c == '0' || c.is_ascii_digit()
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

/// Is `s` plausibly the start of an ARMv7 GPR? Includes the
/// numbered forms (`r0..r15`), the named ones (`sp`, `lr`, `pc`),
/// and partial prefixes of those so we don't kill candidates
/// mid-type.
fn looks_like_arm_reg(s: &str, _sp_allowed: bool) -> bool {
    // Named regs (and their 1-char prefixes).
    if matches!(s, "s" | "sp" | "l" | "lr" | "p" | "pc") {
        return true;
    }
    // `r`, `r0`, `r12`, …
    if let Some(rest) = s.strip_prefix('r') {
        return rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Strict ARMv7-only tokens: text that can ONLY be an ARMv7
/// register (used to kill AArch64 variants). Note `s`/`sp` is
/// shared between ISAs so we exclude it here.
fn looks_like_arm_only_token(s: &str) -> bool {
    // r0..r15 or `r` followed by digits.
    if let Some(rest) = s.strip_prefix('r') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    matches!(s, "lr" | "pc")
}

/// Strict AArch64-only tokens. `w0`, `x0`, `wzr`, `xzr`, `v0`,
/// `b0`/`h0`/`s0`/`d0`/`q0` followed by digits — any of these
/// can't be ARMv7 register names.
fn looks_like_aarch64_only_token(s: &str) -> bool {
    // w<digit>+ or x<digit>+ but NOT bare "w"/"x" (those are
    // also ambiguous in-progress typing).
    if let Some(rest) = s.strip_prefix('w') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
        if rest == "zr" || rest == "sp" {
            return true;
        }
    }
    if let Some(rest) = s.strip_prefix('x') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
        if rest == "zr" {
            return true;
        }
    }
    // v0..v31 vector regs.
    if let Some(rest) = s.strip_prefix('v') {
        if !rest.is_empty() && rest.chars().take_while(|c| c.is_ascii_digit()).count() > 0 {
            return true;
        }
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
        let m = match_variants("mov", 200);
        assert!(!m.is_empty());
        // Includes both AArch64 and ARMv7 mov variants.
        let has_aa64 = m.iter().any(|c| c.variant.isa == VariantIsa::Aarch64);
        let has_arm = m
            .iter()
            .any(|c| matches!(c.variant.isa, VariantIsa::ArmThumb | VariantIsa::ArmA32));
        assert!(has_aa64, "mov should include at least one AArch64 variant");
        assert!(has_arm, "mov should include at least one ARMv7 variant");
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

    // ---- ARMv7 autocomplete tests ----

    #[test]
    fn arm_mov_r1_drops_aarch64() {
        let m = match_variants("mov r1, ", 200);
        assert!(!m.is_empty(), "should still have ARMv7 candidates");
        for c in &m {
            assert!(
                matches!(c.variant.isa, VariantIsa::ArmThumb | VariantIsa::ArmA32),
                "expected ARMv7 only, got {:?} ({})",
                c.variant.isa,
                c.variant.template
            );
        }
    }

    #[test]
    fn arm_bxeq_present() {
        let m = match_variants("bxeq", 50);
        assert!(
            m.iter()
                .any(|c| matches!(c.variant.isa, VariantIsa::ArmThumb | VariantIsa::ArmA32)),
            "bxeq should match at least one ARMv7 variant"
        );
    }

    #[test]
    fn arm_push_shows_reglist() {
        let m = match_variants("push", 50);
        assert!(
            m.iter().any(|c| c.variant.template.contains("{regs}")),
            "push should surface a variant with the {{regs}} placeholder"
        );
    }

    #[test]
    fn aarch64_w0_drops_arm() {
        let m = match_variants("mov w0, ", 200);
        assert!(!m.is_empty(), "should still have AArch64 candidates");
        for c in &m {
            assert!(
                c.variant.isa == VariantIsa::Aarch64,
                "expected AArch64 only, got {:?} ({})",
                c.variant.isa,
                c.variant.template
            );
        }
    }
}
