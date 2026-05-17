//! Row model for the hex view.
//!
//! Pure data + builders extracted from the monolithic `lib.rs`. The
//! per-row renderer still lives in `lib.rs` because it touches
//! `Shell`, the listing palette, and the row-click `RowCtx` — those
//! aren't separable without a bigger refactor. The data structures
//! here, however, have no UI dependencies and are safe to share.

use gpui::SharedString;

/// One precomputed row in a Hex tab's row list.
#[derive(Clone, Debug)]
pub enum HexRow {
    SymbolHeader { name: SharedString },
    Bytes {
        /// Absolute address of the first byte in the row.
        address: u64,
        /// Up to 16 bytes from the section. Length may be less than
        /// 16 for the final row if the section size isn't 16-aligned.
        bytes: Vec<u8>,
    },
}

/// Walk a non-text section emitting hex rows interleaved with symbol
/// headers. One row per 16 bytes plus a header at each symbol entry.
pub fn build_hex_rows(
    data: &crate::DataSectionBytes,
    symbols: &glass_arch_arm64::SymbolMap,
) -> Vec<HexRow> {
    let n = data.row_count();
    let mut rows = Vec::with_capacity(n + n / 16);

    for row_ix in 0..n {
        let addr = data.row_addr(row_ix);
        let row_bytes = data.row_bytes(row_ix);
        let row_end = addr + row_bytes.len() as u64;

        for sym in symbols.in_range(addr, row_end) {
            rows.push(HexRow::SymbolHeader {
                name: SharedString::from(sym.display_name.clone()),
            });
        }

        rows.push(HexRow::Bytes {
            address: addr,
            bytes: row_bytes.to_vec(),
        });
    }

    rows
}

/// Find the row index whose byte range contains `addr`, or the
/// nearest row at or below it. Used to scroll to a clicked address.
pub fn hex_row_for_addr(rows: &[HexRow], addr: u64) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, r) in rows.iter().enumerate() {
        if let HexRow::Bytes { address, bytes } = r {
            let end = address + bytes.len() as u64;
            if *address <= addr && addr < end {
                return Some(i);
            }
            if *address <= addr {
                best = Some(i);
            } else {
                break;
            }
        }
    }
    best
}

