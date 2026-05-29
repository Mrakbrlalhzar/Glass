//! Disasm-row instruction editor.
//!
//! Used to live as an in-place `impl Shell` block at the bottom of
//! `shell_actions.rs`. Hoisted into its own module so the editor
//! has a single home and `shell_actions.rs` stays the place for
//! state-mutation glue (tabs, palette, navigation, …) rather than
//! the in-row text editor.
//!
//! The methods are still defined on `Shell` via a sibling `impl
//! Shell` block — Rust accepts multiple `impl Shell` blocks across
//! files in the same crate, so the call sites in `lib.rs` and
//! `shell_render.rs` keep working without renames.
//!
//! Public-to-`crate` helpers live at module scope so the rest of
//! the UI (notably `changes_dialog`) can name them via
//! `crate::editor::decode_insn_pretty(…)`.

use gpui::Context;

use crate::Shell;

impl Shell {
    pub(crate) fn begin_disasm_edit_at_address(
        &mut self,
        artifact: glass_db::ArtifactId,
        address: u64,
        cx: &mut Context<Self>,
    ) {
        let initial = match self
            .bundle()
            .and_then(|b| b.bytes_at(&artifact, address))
        {
            Some(bytes) => decode_insn_pretty(&bytes, address),
            None => return,
        };
        self.begin_disasm_edit(artifact, address, initial, cx);
    }

    /// Enter edit mode for the disasm row at `(artifact, address)`.
    /// Pre-populates the input with the original mnemonic + operands.
    pub(crate) fn begin_disasm_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        address: u64,
        initial_text: String,
        cx: &mut Context<Self>,
    ) {
        let mut input = crate::text_input::TextInput::from_text(initial_text);
        // Select the whole pre-populated text so the first
        // keystroke replaces it — this is the common case for
        // editing a disasm row (overtype rather than amend).
        input.select_all_pub();
        self.disasm_edit = Some(crate::DisasmEditState {
            artifact,
            address,
            input,
            error: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
        });
        self.refresh_disasm_edit_suggestions();
        cx.notify();
    }

    /// Forward a key event into the active disasm edit's TextInput.
    /// Returns `true` if the edit consumed the event (any printable
    /// key, arrow keys, etc.). Returns `false` for unhandled keys so
    /// the caller can decide what to do.
    pub(crate) fn disasm_edit_handle_key(
        &mut self,
        k: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
        // Up/Down navigate the suggestion list when one is showing.
        // Tab commits the highlighted suggestion. Other keys
        // forward to the TextInput.
        if k.key == "up" {
            self.move_disasm_suggestion(-1, cx);
            return true;
        }
        if k.key == "down" {
            self.move_disasm_suggestion(1, cx);
            return true;
        }
        if k.key == "tab" {
            self.commit_disasm_suggestion(cx);
            return true;
        }
        let Some(edit) = self.disasm_edit.as_mut() else {
            return false;
        };
        let shift = k.modifiers.shift;
        let cmd = k.modifiers.platform || k.modifiers.control;
        let alt = k.modifiers.alt;
        let key_char = k.key_char.as_deref();
        let app: &mut gpui::App = &mut *cx;
        let _changed = edit
            .input
            .handle_key(&k.key, shift, cmd, alt, key_char, app);
        edit.error = None;
        self.refresh_disasm_edit_suggestions();
        cx.notify();
        true
    }

    /// Mouse-click handler for a suggestion row. Highlights the
    /// clicked index and commits it. Used by the suggestion
    /// overlay's per-row click handlers.
    pub(crate) fn click_disasm_suggestion(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(e) = self.disasm_edit.as_mut() {
            if index < e.suggestions.len() {
                e.suggestion_selected = index;
            } else {
                return;
            }
        } else {
            return;
        }
        self.commit_disasm_suggestion(cx);
    }

    pub(crate) fn move_disasm_suggestion_pub(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.move_disasm_suggestion(delta, cx);
    }

    fn move_disasm_suggestion(&mut self, delta: i32, cx: &mut Context<Self>) {
        if let Some(e) = self.disasm_edit.as_mut() {
            if e.suggestions.is_empty() {
                return;
            }
            let max = e.suggestions.len() - 1;
            let next = (e.suggestion_selected as i32 + delta).clamp(0, max as i32) as usize;
            if next != e.suggestion_selected {
                e.suggestion_selected = next;
                cx.notify();
            }
        }
    }

    pub(crate) fn commit_disasm_suggestion_pub(&mut self, cx: &mut Context<Self>) {
        self.commit_disasm_suggestion(cx);
    }

    /// Replace the partial word at the cursor with the
    /// highlighted suggestion's `commit_text`. Cursor lands at
    /// the end of the inserted text. The dropdown is dismissed
    /// until the next keystroke so the user gets a clear path
    /// to "Enter commits the whole edit" — if we re-classified
    /// immediately, suggestions for the *next* slot would pop
    /// up and Enter would never reach `commit_disasm_edit`.
    fn commit_disasm_suggestion(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.disasm_edit.as_mut() else { return };
        let Some(sugg) = edit.suggestions.get(edit.suggestion_selected).cloned()
        else {
            return;
        };
        let text = edit.input.text().to_string();
        let cursor = edit.input.cursor();
        let ctx = glass_api::classify_insn_cursor(&text, cursor);
        let mut new_text = String::with_capacity(text.len() + sugg.commit_text.len());
        new_text.push_str(&text[..ctx.word_range.start]);
        new_text.push_str(&sugg.commit_text);
        new_text.push_str(&text[ctx.word_range.end..]);
        edit.input.set_text(new_text);
        edit.suggestions.clear();
        edit.suggestion_selected = 0;
        cx.notify();
    }

    /// Rebuild the suggestion list based on the current input +
    /// cursor. Cheap — symbol scans are linear over the
    /// artifact's symbol map (typically a few thousand entries).
    pub(crate) fn refresh_disasm_edit_suggestions(&mut self) {
        // Pull input + artifact before grabbing the mutable
        // borrow so we can still read `self.bundle()` while
        // building the suggestion list.
        let (text, artifact) = match self.disasm_edit.as_ref() {
            Some(e) => (e.input.text().to_string(), e.artifact.clone()),
            None => return,
        };
        let cursor = self
            .disasm_edit
            .as_ref()
            .map(|e| e.input.cursor())
            .unwrap_or(text.len());
        let ctx = glass_api::classify_insn_cursor(&text, cursor);
        let mut out: Vec<crate::EditSuggestion> = Vec::new();
        match ctx.kind {
            glass_api::CursorKind::Mnemonic => {
                let variants = glass_api::match_insn_variants(&ctx.partial, 12);
                for cand in variants {
                    out.push(crate::EditSuggestion {
                        label: cand.variant.template.clone().into(),
                        commit_text: cand.variant.mnemonic.to_string(),
                        detail: "asm".into(),
                        kind: crate::EditSuggestionKind::Mnemonic,
                    });
                }
            }
            glass_api::CursorKind::BranchTargetSlot => {
                if let Some(bundle) = self.bundle() {
                    if let Some(map) = bundle.symbol_maps.get(&artifact) {
                        let needle = ctx.partial.as_str();
                        let mut hits: Vec<(usize, _)> = Vec::new();
                        for sym in map.iter() {
                            let display_lower = sym.display_name.to_ascii_lowercase();
                            let raw_lower = sym.name.to_ascii_lowercase();
                            let score = if display_lower.starts_with(needle) {
                                0
                            } else if raw_lower.starts_with(needle) {
                                1
                            } else if display_lower.contains(needle) {
                                2
                            } else if raw_lower.contains(needle) {
                                3
                            } else {
                                continue;
                            };
                            hits.push((score, sym));
                        }
                        hits.sort_by_key(|(s, _)| *s);
                        hits.truncate(20);
                        for (_, sym) in hits {
                            out.push(crate::EditSuggestion {
                                label: sym.display_name.clone().into(),
                                commit_text: sym.display_name.clone(),
                                detail: format!("0x{:x}", sym.address).into(),
                                kind: crate::EditSuggestionKind::Symbol,
                            });
                        }
                    }
                }
            }
            glass_api::CursorKind::RegisterSlot => {
                let needle = ctx.partial.as_str();
                for &name in REGISTER_NAMES {
                    if name.starts_with(needle) {
                        out.push(crate::EditSuggestion {
                            label: name.into(),
                            commit_text: name.to_string(),
                            detail: "reg".into(),
                            kind: crate::EditSuggestionKind::Register,
                        });
                        if out.len() >= 20 {
                            break;
                        }
                    }
                }
            }
            glass_api::CursorKind::ImmediateSlot | glass_api::CursorKind::MemorySlot => {
                // No suggestions; raw input.
            }
        }
        if let Some(edit) = self.disasm_edit.as_mut() {
            edit.suggestions = out;
            edit.suggestion_selected = 0;
        }
    }

    /// Try to compile + stage the in-progress edit. On success the
    /// edit is added to the bundle's `edits` registry; on failure
    /// the error chip is updated and edit mode stays open.
    pub(crate) fn commit_disasm_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.disasm_edit.clone() else { return };
        let source_text = edit.input.text().to_string();
        // Look up the original bytes from the bundle so we can
        // store them for revert / dialog rendering.
        let Some(bundle) = self.bundle() else { return };
        // Decide arch + original instruction width. ARMv7 sections
        // have a precomputed vector so we can look up the typed
        // instruction at `addr`; AArch64 sections fall through to
        // the fixed-4-byte path used historically.
        let is_armv7 = bundle.is_armv7_text(&edit.artifact, edit.address);
        let (original_bytes, original_width, prefer_thumb) = if is_armv7 {
            match bundle.precomputed_insn_at(&edit.artifact, edit.address) {
                Some(insn) => {
                    let raw = insn.raw_bytes();
                    let width = raw.len();
                    let is_thumb = matches!(
                        insn,
                        glass_arch_arm::DecodedInsn::Thumb(_)
                    );
                    (raw, width, is_thumb)
                }
                None => {
                    if let Some(e) = self.disasm_edit.as_mut() {
                        e.error = Some(format!(
                            "no instruction starts at 0x{:x} (mid-instruction edit?)",
                            edit.address
                        ));
                    }
                    cx.notify();
                    return;
                }
            }
        } else {
            // AArch64: fixed 4-byte word from `bytes_at`.
            match bundle.bytes_at(&edit.artifact, edit.address) {
                Some(b) => (b.to_vec(), 4usize, false),
                None => {
                    if let Some(e) = self.disasm_edit.as_mut() {
                        e.error = Some(format!("no instruction at 0x{:x}", edit.address));
                    }
                    cx.notify();
                    return;
                }
            }
        };
        // Compile with the row's address (so PC-relative
        // encodings come out correctly) and a symbol resolver
        // sourced from the artifact's symbol map. Both AArch64
        // and ARMv7 take the same closure shape so we build it
        // once.
        let sym_map = bundle.symbol_maps.get(&edit.artifact).cloned();
        let lookup: Box<dyn Fn(&str) -> Option<u64>> = match sym_map {
            Some(map) => Box::new(move |needle: &str| {
                map.iter()
                    .find(|s| s.display_name == needle || s.name == needle)
                    .map(|s| s.address)
            }),
            None => Box::new(|_| None),
        };
        let compile_result: anyhow::Result<Vec<u8>> = if is_armv7 {
            glass_api::compile_armv7_at(
                &source_text,
                edit.address,
                prefer_thumb,
                Some(lookup.as_ref()),
            )
        } else {
            glass_api::compile_insn_at(&source_text, edit.address, Some(lookup.as_ref()))
        };
        let raw_new_bytes = match compile_result {
            Ok(b) => b,
            Err(err) => {
                if let Some(e) = self.disasm_edit.as_mut() {
                    e.error = Some(format!("{err:#}"));
                }
                cx.notify();
                return;
            }
        };
        // Width classification: same-width / shrink / grow-with-
        // nop-absorption / refuse-anything-else. AArch64 and
        // ARM-mode A32 are fixed-4-byte so only the same-width
        // case applies; Thumb is variable (2 or 4).
        let new_len = raw_new_bytes.len();
        if new_len != 2 && new_len != 4 {
            if let Some(e) = self.disasm_edit.as_mut() {
                e.error = Some(format!(
                    "encoder produced {new_len} bytes; only 2- and 4-byte instructions are supported"
                ));
            }
            cx.notify();
            return;
        }
        let (final_new_bytes, absorbed_following) = match new_len.cmp(&original_width) {
            std::cmp::Ordering::Equal => (raw_new_bytes, 0u8),
            std::cmp::Ordering::Less => {
                // Shrink. Only legal in Thumb (variable-width).
                // ARM-mode A32 and AArch64 are uniform 4 bytes, so
                // the only legal shrink is 4→2 with prefer_thumb.
                if !(is_armv7 && prefer_thumb && original_width == 4 && new_len == 2) {
                    if let Some(e) = self.disasm_edit.as_mut() {
                        e.error = Some(format!(
                            "can't shrink a {original_width}-byte instruction to {new_len} bytes in this section"
                        ));
                    }
                    cx.notify();
                    return;
                }
                // Pad the trailing slot with a Thumb-1 NOP
                // (`0xbf 0x00`). The listing's next paint walks
                // the original-bytes layout, so this gives the
                // user a clean "instruction + explicit nop" pair.
                let mut padded = raw_new_bytes;
                padded.push(0xbf);
                padded.push(0x00);
                (padded, 0u8)
            }
            std::cmp::Ordering::Greater => {
                // Grow. Only legal if the following slot is a
                // Thumb-1 NOP we can absorb (2→4). Anything else
                // refuses — branch-rebinding / downstream shift
                // is out of scope for this pass.
                if !(is_armv7 && prefer_thumb && original_width == 2 && new_len == 4) {
                    if let Some(e) = self.disasm_edit.as_mut() {
                        e.error = Some(format!(
                            "can't grow a {original_width}-byte instruction to {new_len} bytes without downstream shifting"
                        ));
                    }
                    cx.notify();
                    return;
                }
                let next_addr = edit.address.saturating_add(original_width as u64);
                let next_is_nop = bundle
                    .precomputed_insn_at(&edit.artifact, next_addr)
                    .map(|i| i.is_thumb1_nop())
                    .unwrap_or(false);
                if !next_is_nop {
                    if let Some(e) = self.disasm_edit.as_mut() {
                        e.error = Some(format!(
                            "2-byte → 4-byte edit needs an adjacent NOP at 0x{next_addr:x} (none found)"
                        ));
                    }
                    cx.notify();
                    return;
                }
                (raw_new_bytes, 2u8)
            }
        };
        // Decode the new bytes for the cached pretty-print.
        // AArch64 goes through the existing decode helper; ARMv7
        // routes to `decode_armv7_pretty_with_symbols` so the
        // staged-row display canonicalises what the user typed and
        // resolves branch targets to symbol names where possible.
        let display = if is_armv7 {
            let sym_map_for_display = bundle.symbol_maps.get(&edit.artifact).cloned();
            decode_armv7_pretty_with_symbols(
                &final_new_bytes,
                edit.address,
                prefer_thumb,
                |addr: u64| {
                    sym_map_for_display
                        .as_ref()
                        .and_then(|m| m.at(addr).map(|s| s.display_name.clone()))
                },
            )
        } else {
            let sym_map_for_display = bundle.symbol_maps.get(&edit.artifact).cloned();
            let mut bytes4 = [0u8; 4];
            let n = final_new_bytes.len().min(4);
            bytes4[..n].copy_from_slice(&final_new_bytes[..n]);
            decode_insn_pretty_with_symbols(
                &bytes4,
                edit.address,
                |addr: u64| {
                    sym_map_for_display
                        .as_ref()
                        .and_then(|m| m.at(addr).map(|s| s.display_name.clone()))
                },
            )
        };
        let staged = crate::edits::Edit {
            artifact: edit.artifact.clone(),
            vaddr: edit.address,
            kind: crate::edits::EditKind::Instruction,
            new_bytes: final_new_bytes,
            original_bytes,
            source_text,
            display,
            absorbed_following,
        };
        if let Some(b) = self.bundle_mut() {
            b.edits.insert(staged);
        }
        self.disasm_edit = None;
        cx.notify();
    }

    pub(crate) fn cancel_disasm_edit(&mut self, cx: &mut Context<Self>) {
        if self.disasm_edit.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn revert_disasm_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        vaddr: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(b) = self.bundle_mut() {
            b.edits.remove(&artifact, vaddr);
        }
        cx.notify();
    }
}

// ---- Module-scope decode helpers --------------------------------

/// Pretty-print a 4-byte instruction at `addr`. Used by edit
/// staging to cache the disasm of the new bytes so the listing
/// renderer + Changes dialog don't have to re-decode on every
/// paint. PC-relative operands are resolved against `addr` so
/// branches show their real target.
pub(crate) fn decode_insn_pretty(bytes: &[u8; 4], addr: u64) -> String {
    decode_insn_pretty_with_symbols(bytes, addr, |_| None)
}

/// Variant that resolves address operands to symbol names via
/// the supplied closure. Used after staging an edit so the
/// "modified" row shows `bl decode_packet` instead of `bl 0x…`.
pub(crate) fn decode_insn_pretty_with_symbols<F>(
    bytes: &[u8; 4],
    addr: u64,
    symbol_for_address: F,
) -> String
where
    F: Fn(u64) -> Option<String>,
{
    let word = u32::from_le_bytes(*bytes);
    match armv8_encode::isa::aarch64::decode_instruction(addr, word) {
        Ok(insn) => {
            let mnem = insn.format_mnemonic();
            let ops = insn.format_operands_with_symbols(symbol_for_address);
            if ops.is_empty() {
                mnem
            } else {
                format!("{mnem} {ops}")
            }
        }
        Err(_) => format!(".word 0x{word:08x}"),
    }
}

/// ARMv7 sibling of [`decode_insn_pretty_with_symbols`]. Decodes
/// the variable-width bytes (2 or 4 for Thumb, 4 for ARM) and
/// renders the canonical assembly, resolving any branch / PC-rel
/// operand via the supplied closure. The `prefer_thumb` flag picks
/// the decoder lane the same way `compile_armv7_at` does. Used by
/// the disasm editor to canonicalise what the user typed after a
/// commit, so the staged-row display reads identically to a fresh
/// disasm of the new bytes.
pub(crate) fn decode_armv7_pretty_with_symbols<F>(
    bytes: &[u8],
    addr: u64,
    prefer_thumb: bool,
    symbol_for_address: F,
) -> String
where
    F: Fn(u64) -> Option<String>,
{
    use armv8_encode::isa::armv7;
    if prefer_thumb {
        // Thumb-1 = 2 bytes, Thumb-2 = 4 bytes. read_instruction
        // returns (word, width); fall back to a `.word` if either
        // table lookup misses.
        if let Ok((word, width)) = armv7::read_instruction(bytes, 0) {
            use armv7::table::ThumbWidth;
            let tw = if width == 4 { ThumbWidth::Word } else { ThumbWidth::Halfword };
            if let Some(row) = armv7::table_generated::match_generated(word, tw) {
                let (operands, _unhandled) =
                    armv7::format_decode::decode_operands_from_format(
                        row.format, word, addr, width,
                    );
                let insn = armv7::sweep::ThumbDecodedInstruction {
                    address: addr,
                    word,
                    width: tw,
                    mnemonic: row.mnemonic,
                    operands,
                    row: Some(row),
                    neon_row: None,
                };
                return glass_arch_arm::arm_format::format_thumb_with_symbols(
                    &insn,
                    symbol_for_address,
                );
            }
            return match width {
                2 => format!(".hword 0x{:04x}", word & 0xffff),
                _ => format!(".word 0x{word:08x}"),
            };
        }
        return ".word ??".to_string();
    }
    if bytes.len() < 4 {
        return ".word ??".to_string();
    }
    let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if let Some(row) = armv7::arm::table_generated::match_generated(word) {
        let (operands, _unhandled) =
            armv7::arm::format_decode::decode_operands_from_format(row.format, word, addr);
        let insn = armv7::arm::sweep::ArmDecodedInstruction {
            address: addr,
            word,
            mnemonic: row.mnemonic,
            operands,
            row: Some(row),
            neon_row: None,
        };
        return glass_arch_arm::arm_format::format_arm_with_symbols(
            &insn,
            symbol_for_address,
        );
    }
    format!(".word 0x{word:08x}")
}

/// Register names offered by the edit-mode autocomplete when
/// the cursor sits in a register slot. Order: zero registers,
/// stack pointers, then numeric W/X in ascending order. The
/// dropdown shows a prefix-filtered, capped subset.
const REGISTER_NAMES: &[&str] = &[
    "wzr", "xzr", "wsp", "sp",
    "w0", "w1", "w2", "w3", "w4", "w5", "w6", "w7", "w8", "w9",
    "w10", "w11", "w12", "w13", "w14", "w15", "w16", "w17", "w18", "w19",
    "w20", "w21", "w22", "w23", "w24", "w25", "w26", "w27", "w28", "w29", "w30",
    "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9",
    "x10", "x11", "x12", "x13", "x14", "x15", "x16", "x17", "x18", "x19",
    "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28", "x29", "x30",
];
