//! Per-screen copy-to-clipboard support.
//!
//! Hosts `Shell::current_copy_text` (the dispatch on the active tab
//! that produces a human-readable string for the "selected thing")
//! and `Shell::copy_current_to_clipboard` (which writes the result
//! to the system clipboard via gpui).
//!
//! Text inputs (rename / comment / palette query / disasm-edit
//! overlay) already implement their own cut/copy/paste inside
//! `text_input::TextInput`; the action wired to `cmd-c` here only
//! fires when no focused input claimed the keystroke first, which
//! is gpui's default action-dispatch behaviour.
//!
//! Right-click "Copy" menu entries reuse the same formatters
//! through `ContextMenuItem::CopyText`.

use gpui::{ClipboardItem, Context};

use crate::hex::HexRow;
use crate::listing_model::ListingRow;
use crate::{Shell, TabKind};

impl Shell {
    /// Write `current_copy_text()` to the system clipboard. No-op if
    /// nothing is selected — we deliberately don't pop an error
    /// because cmd-c on an empty selection is a common accident.
    pub(crate) fn copy_current_to_clipboard(&self, cx: &mut Context<Self>) {
        if let Some(text) = self.current_copy_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Format the "selected thing" for the active tab. Returns
    /// `None` when there's no well-defined selection. The format
    /// mirrors what the user sees on screen (column-aligned for
    /// listing/hex, raw line text for smali/manifest, a one-line
    /// summary for sectionmap).
    /// Helper used by `open_listing_context_menu` to format the
    /// right-clicked row regardless of which tab is currently
    /// active. Walks every tab whose kind is Listing on the given
    /// artifact looking for one that has rendered rows containing
    /// `addr`. Returns `None` if no such tab exists yet (the rows
    /// are built lazily off-thread).
    pub(crate) fn copy_text_for_listing_addr(
        &self,
        artifact: &glass_db::ArtifactId,
        addr: u64,
    ) -> Option<String> {
        for tab in &self.tabs {
            if let TabKind::Listing { artifact: a, .. } = &tab.kind {
                if a != artifact {
                    continue;
                }
                let Some(rows) = tab.listing_rows.as_ref() else { continue };
                if let Some(idx) = crate::listing_model::listing_row_for_addr(rows, addr) {
                    if let Some(row) = rows.get(idx) {
                        return Some(format_listing_row(row));
                    }
                }
            }
        }
        None
    }

    pub(crate) fn current_copy_text(&self) -> Option<String> {
        let active = self.active_tab?;
        let tab = self.tabs.get(active)?;
        match &tab.kind {
            TabKind::Listing { .. } => copy_listing(tab),
            TabKind::Hex { .. } => copy_hex(tab),
            TabKind::SectionMap { artifact } => copy_section_map(self, artifact),
            // ManifestEditor uses the rope-backed selection like the
            // other editor tabs — handled below.
            TabKind::ManifestEditor { .. } => {
                tab.code_editor.as_ref().and_then(|e| e.selected_text())
            }
            // CFG and DexCallGraph don't carry a per-node selection
            // state today. TODO: when node-selection lands, plumb it
            // through here.
            TabKind::Cfg { .. } | TabKind::DexCallGraph { .. } => None,
            // ObjC class view doesn't carry an editable per-row
            // selection clipboard payload yet — same shape as the
            // CFG cases above.
            TabKind::ObjCClass { .. } => None,
            // Swift class view: same TODO as ObjC — needs a
            // per-row copy formatter once the renderer learns
            // a notion of selectable units.
            TabKind::SwiftType { .. } => None,
            // Script + Smali editors: the rope-backed selection
            // model is the source of truth — return its
            // currently-selected text so the global Cmd-C action
            // can place it on the clipboard.
            TabKind::ScriptEditor { .. }
            | TabKind::SmaliEditor { .. }
            | TabKind::PlistEditor { .. } => {
                tab.code_editor.as_ref().and_then(|e| e.selected_text())
            }
            // Coverage map has no row-level selection model
            // yet. Cmd-C in the tab is a no-op.
            TabKind::CoverageMap => None,
        }
    }
}

fn copy_listing(tab: &crate::Tab) -> Option<String> {
    let row_idx = tab.selected_row?;
    let rows = tab.listing_rows.as_ref()?;
    let row = rows.get(row_idx)?;
    Some(format_listing_row(row))
}

/// Format a listing row the way the listing view shows it:
/// `0xADDR  bytes  mnemonic operands  ; comment` for an
/// instruction; `name:` for a symbol header; an empty separator
/// gets a single space (still copy-able so the user gets visual
/// feedback).
pub(crate) fn format_listing_row(row: &ListingRow) -> String {
    match row {
        ListingRow::SymbolHeader { name } => format!("{name}:"),
        ListingRow::BasicBlockSeparator { .. } => String::new(),
        ListingRow::Instruction {
            address,
            bytes,
            len,
            mnemonic,
            operands,
            comment,
            ..
        } => {
            let bytes_col = format_bytes_for_copy(bytes, *len);
            let ops = operands
                .iter()
                .map(|c| c.text.as_str())
                .collect::<String>();
            let mut out = format!("0x{address:x}  {bytes_col}  {mnemonic}");
            if !ops.is_empty() {
                out.push(' ');
                out.push_str(&ops);
            }
            if !comment.is_empty() {
                out.push_str("  ; ");
                out.push_str(comment);
            }
            out
        }
    }
}

/// Mirrors `listing_render::format_bytes_column` but lives here so
/// the copy path doesn't depend on the renderer. Two hex chars per
/// byte, single-space separated, trailing slots become two spaces
/// each so column widths stay stable across 2/4-byte rows.
fn format_bytes_for_copy(bytes: &[u8; 4], len: u8) -> String {
    use std::fmt::Write;
    let n = (len as usize).min(4);
    let mut out = String::with_capacity(11);
    for i in 0..4 {
        if i > 0 {
            out.push(' ');
        }
        if i < n {
            let _ = write!(out, "{:02x}", bytes[i]);
        } else {
            out.push_str("  ");
        }
    }
    out
}

fn copy_hex(tab: &crate::Tab) -> Option<String> {
    let rows = tab.hex_rows.as_ref()?;
    // Prefer single-byte selection when set; fall back to the full
    // row at `selected_row`.
    if let Some(byte_addr) = tab.selected_byte_addr {
        let row_idx = crate::hex::hex_row_for_addr(rows, byte_addr)?;
        if let HexRow::Bytes { address, bytes } = rows.get(row_idx)? {
            let offset = byte_addr.checked_sub(*address)? as usize;
            let byte = *bytes.get(offset)?;
            return Some(format!("0x{byte_addr:08x}: {byte:02x}"));
        }
        return None;
    }
    let row_idx = tab.selected_row?;
    match rows.get(row_idx)? {
        HexRow::Bytes { address, bytes } => {
            let mut s = format!("0x{address:08x}:");
            for b in bytes {
                use std::fmt::Write;
                let _ = write!(s, " {b:02x}");
            }
            Some(s)
        }
        HexRow::SymbolHeader { name } => Some(format!("{name}:")),
    }
}

fn copy_section_map(shell: &Shell, artifact: &glass_db::ArtifactId) -> Option<String> {
    let idx = shell.hovered_section?;
    let bundle = shell.bundle()?;
    let sections = bundle.native_sections.get(artifact)?;
    let sec = sections.get(idx)?;
    Some(format!(
        "{}  base=0x{:x}  size=0x{:x}",
        sec.name, sec.address, sec.size
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::SharedString;
    use std::sync::Arc;

    #[test]
    fn instruction_row_includes_addr_bytes_mnem_ops() {
        let row = ListingRow::Instruction {
            address: 0x1000,
            bytes: [0x00, 0x00, 0x80, 0xd2],
            len: 4,
            mnemonic: SharedString::from("mov"),
            operands: Arc::new(vec![glass_arch_arm::Chunk {
                text: "x0, #0".into(),
                kind: glass_arch_arm::ChunkKind::Plain,
                target: None,
                target_text: None,
            }]),
            comment: SharedString::from(""),
            arrows: Arc::new(Vec::new()),
        };
        let s = format_listing_row(&row);
        assert!(s.starts_with("0x1000"));
        assert!(s.contains("00 00 80 d2"));
        assert!(s.contains("mov"));
        assert!(s.contains("x0, #0"));
        assert!(!s.contains(';'));
    }

    #[test]
    fn instruction_row_with_comment_has_semicolon() {
        let row = ListingRow::Instruction {
            address: 0x2000,
            bytes: [0x1f, 0x20, 0x03, 0xd5],
            len: 4,
            mnemonic: SharedString::from("nop"),
            operands: Arc::new(Vec::new()),
            comment: SharedString::from("padding"),
            arrows: Arc::new(Vec::new()),
        };
        let s = format_listing_row(&row);
        assert!(s.ends_with("; padding"));
    }

    #[test]
    fn thumb_two_byte_row_blanks_trailing_slots() {
        let row = ListingRow::Instruction {
            address: 0x3000,
            bytes: [0xaa, 0xbb, 0x00, 0x00],
            len: 2,
            mnemonic: SharedString::from("bx"),
            operands: Arc::new(Vec::new()),
            comment: SharedString::from(""),
            arrows: Arc::new(Vec::new()),
        };
        let s = format_listing_row(&row);
        // Two real bytes then two blank slots (two spaces each).
        assert!(s.contains("aa bb       "));
    }

    #[test]
    fn symbol_header_renders_with_colon() {
        let row = ListingRow::SymbolHeader {
            name: SharedString::from("_main"),
        };
        assert_eq!(format_listing_row(&row), "_main:");
    }
}
