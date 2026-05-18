//! Linear-listing row model + builders.
//!
//! `ListingRow`/`ArrowSegment` describe the precomputed virtualised
//! rows the listing view scrolls through. `build_listing_rows` walks a
//! text section's instructions, decodes each one, attaches resolved
//! branch/ADRP targets as a comment, then `assign_arrows` overlays
//! intra-function arrow segments. All UI-free; safe to run on a
//! background thread.

use std::sync::{Arc, Mutex};

use gpui::SharedString;

use crate::{Progress, TextSectionBytes};

/// Maximum gutter lane count. Arrows that would land beyond this are
/// dropped rather than drawn clipped.
pub const ARROW_MAX_LANES: u8 = 5;

pub enum ListingRow {
    /// `<symbol>:` line preceding a symbol entry point.
    SymbolHeader { name: SharedString },
    /// One AArch64 instruction.
    Instruction {
        address: u64,
        bytes: [u8; 4],
        mnemonic: SharedString,
        operands: Arc<Vec<glass_arch_arm64::Chunk>>,
        /// Trailing `; ...` comment chunks. Empty if no annotation.
        comment: SharedString,
        /// Control-flow arrow segments this row contributes to the
        /// listing gutter. Empty for rows not touched by any in-
        /// function branch arrow.
        arrows: Arc<Vec<ArrowSegment>>,
    },
    /// Horizontal rule drawn after a basic-block terminator. Carries
    /// any arrow segments that pass over it so the control-flow lines
    /// remain continuous across BB boundaries.
    BasicBlockSeparator {
        arrows: Arc<Vec<ArrowSegment>>,
    },
}

/// One arrow segment in a listing row's gutter. Each direct branch
/// (B, B.cond, Cbz/Cbnz, Tbz/Tbnz) inside the current function gets
/// assigned a lane; every row between source and target gets the
/// segments needed to draw a continuous line from source → target →
/// arrowhead in that lane.
#[derive(Clone, Debug)]
pub struct ArrowSegment {
    /// 0 = column closest to the address text; larger = further left.
    pub lane: u8,
    /// Solid for unconditional `B`, dotted for conditionals.
    pub style: ArrowStyle,
    /// Where in this row's gutter cell the segment lives.
    pub role: ArrowRole,
    /// Down for forward branches (target is below source in row order);
    /// Up for backward branches. Affects which side of the row the
    /// horizontal stub points and which way the arrowhead faces.
    pub direction: ArrowDirection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowStyle { Solid, Dotted }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowDirection { Down, Up }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowRole {
    /// Source row: horizontal stub from the address column to the lane,
    /// plus a half-height vertical segment heading toward the target.
    Source,
    /// Target row: half-height vertical segment ending at the row
    /// middle, plus a horizontal stub with an arrowhead pointing into
    /// the address column.
    Target,
    /// Row strictly between source and target — full-height vertical.
    Pass,
}
/// Owning snapshot of an artifact's non-text bytes, used by
/// `build_listing_rows` to resolve ADRP+ADD targets to string
/// literals. The bytes are shared via `Arc` so passing this to a
/// worker thread is cheap.
pub struct DataPeek {
    pub sections: Vec<(u64, Arc<Vec<u8>>)>, // (base, bytes)
    /// Parallel list of (name, base, size) — used by section-name
    /// labelling on ADRP target operands. Optional and additive to
    /// the bytes vector above; sections without a name entry just
    /// fall back to "0x…" labelling.
    pub section_meta: Vec<DataSectionMeta>,
}

#[derive(Clone, Debug)]
pub struct DataSectionMeta {
    pub name: String,
    pub base: u64,
    pub size: u64,
}

impl DataPeek {
    pub fn empty() -> Self {
        Self { sections: Vec::new(), section_meta: Vec::new() }
    }

    /// `(section_name, section_base)` containing `addr`, if known.
    /// Lets the renderer label an ADRP page address with
    /// `__section_name+0x<offset>` when no covering symbol exists.
    pub fn section_containing(&self, addr: u64) -> Option<(&str, u64)> {
        for s in &self.section_meta {
            if addr >= s.base && addr < s.base.saturating_add(s.size) {
                return Some((s.name.as_str(), s.base));
            }
        }
        None
    }

    /// Best-effort ASCII string peek starting at `addr`. Returns up to
    /// `max_len` printable characters, terminated by a NUL or the
    /// first non-printable byte. `None` if `addr` doesn't fall in any
    /// known section, or the first byte isn't a printable ASCII.
    pub fn peek_string(&self, addr: u64, max_len: usize) -> Option<String> {
        // Walk every section that covers `addr` and return the first
        // that yields a valid printable run. Sections sometimes
        // overlap (especially when an artifact carries debug-info
        // copies of real data), so we can't short-circuit after the
        // first containing section without missing valid strings in
        // a different section that also contains the same address.
        for (base, bytes) in &self.sections {
            if addr < *base || addr >= base + bytes.len() as u64 {
                continue;
            }
            let off = (addr - base) as usize;
            let slice = &bytes[off..];
            if !slice.first().is_some_and(|b| (0x20..=0x7e).contains(b)) {
                continue;
            }
            let mut out = String::new();
            let mut ok = true;
            for &b in slice.iter().take(max_len) {
                if b == 0 {
                    break;
                }
                if !(0x20..=0x7e).contains(&b) {
                    ok = false;
                    break;
                }
                out.push(b as char);
            }
            if ok && out.len() >= 2 {
                return Some(out);
            }
        }
        None
    }
}
/// X-register indices in the decoded operands, in order they appear.
/// SP shares an index space with the GP registers via RegisterClass,
/// but ADRP/ADD targets are always GP X-registers in practice.
fn x_regs_of(insn: &armv8_encode::isa::aarch64::DecodedInstruction) -> Vec<u8> {
    use armv8_encode::isa::aarch64::{DecodedOperand, RegisterClass};
    let mut out = Vec::with_capacity(insn.operands.len());
    for op in &insn.operands {
        if let DecodedOperand::Register(r) = op {
            if matches!(r.class, RegisterClass::X | RegisterClass::XOrSp) {
                out.push(r.index);
            }
        }
    }
    out
}

/// Pull an immediate value out of an instruction's operands. Supports
/// plain Immediate, UnsignedImmediate and ShiftedImmediate. None if
/// there's no immediate operand.
fn first_imm_of(insn: &armv8_encode::isa::aarch64::DecodedInstruction) -> Option<i64> {
    use armv8_encode::isa::aarch64::DecodedOperand;
    for op in &insn.operands {
        match op {
            DecodedOperand::Immediate(v) => return Some(*v),
            DecodedOperand::UnsignedImmediate(v) => return Some(*v as i64),
            DecodedOperand::ShiftedImmediate(s) => {
                return Some(s.value.wrapping_shl(s.shift as u32))
            }
            _ => {}
        }
    }
    None
}

/// If `insn` is `adrp Xd, target`, return `(d_index, target)`.
fn extract_adrp(
    insn: &armv8_encode::isa::aarch64::DecodedInstruction,
) -> Option<(u8, u64)> {
    use armv8_encode::isa::aarch64::{Aarch64Mnemonic, DecodedOperand};
    if insn.mnemonic != Aarch64Mnemonic::Adrp {
        return None;
    }
    let regs = x_regs_of(insn);
    let page = insn.operands.iter().find_map(|op| match op {
        DecodedOperand::PageTarget(a) => Some(*a),
        _ => None,
    });
    Some((*regs.first()?, page?))
}

/// If `insn` is an `add Xd, Xs, #imm` whose `Xs` has a known page base,
/// return `(d_index, s_index, final_addr)`. Returns `None` for any add
/// shape that isn't a simple `Xd <- Xs + immediate`.
fn extract_add_with_imm(
    insn: &armv8_encode::isa::aarch64::DecodedInstruction,
    page_bases: &[Option<u64>; 32],
) -> Option<(u8, u8, u64)> {
    use armv8_encode::isa::aarch64::Aarch64Mnemonic;
    if insn.mnemonic != Aarch64Mnemonic::Add {
        return None;
    }
    let regs = x_regs_of(insn);
    if regs.len() < 2 {
        return None;
    }
    let d = regs[0];
    let s = regs[1];
    let base = page_bases.get(s as usize).copied().flatten()?;
    let imm = first_imm_of(insn)?;
    if imm < 0 {
        return None;
    }
    Some((d, s, base.wrapping_add(imm as u64)))
}

/// Index of the X-register written by `insn`, if any. Used to
/// invalidate stale page bases when the destination gets clobbered by
/// a later instruction. Conservative — we treat the first X-register
/// operand as the destination, which is correct for almost every
/// ARM64 instruction we care about (data-proc, ldr, mov, …).
fn dest_x_reg(insn: &armv8_encode::isa::aarch64::DecodedInstruction) -> Option<u8> {
    x_regs_of(insn).into_iter().next()
}


pub fn build_listing_rows(
    text: &TextSectionBytes,
    symbols: &glass_arch_arm64::SymbolMap,
    data: &DataPeek,
    progress: Option<&Arc<Mutex<Progress>>>,
) -> Vec<ListingRow> {
    use glass_arch_arm64::format as fmt;
    let n = text.instruction_count();
    // Tracks the row index of the ADRP that produced each
    // x_page_bases entry. When a later ADD resolves an ADRP+ADD
    // pair whose resolved target sits in a *different* section
    // from the page address, we use this to retro-label the ADRP
    // row with the destination section (negative offset) so the
    // reader sees the destination at a glance.
    let mut x_page_origin_row: [Option<usize>; 32] = [None; 32];
    if let Some(p) = progress {
        if let Ok(mut p) = p.lock() {
            p.phase = SharedString::from("Disassembling…");
            p.current = 0;
            p.total = n;
        }
    }
    // Rough capacity: ~1.2 rows per insn (some symbol headers + BB
    // separators). Avoids most reallocations on large sections.
    let mut rows = Vec::with_capacity(n + n / 8);

    // ADRP+ADD pair tracking. For each X-register we remember the
    // most recent ADRP page address loaded into it; any later ADD that
    // sources from that register resolves to `page + imm`. We
    // invalidate a slot whenever an instruction writes to its
    // register (the conservative rule — a write loses the page base
    // for further resolution).
    let mut x_page_bases: [Option<u64>; 32] = [None; 32];

    for i in 0..n {
        if i % 1024 == 0 {
            if let Some(p) = progress {
                if let Ok(mut p) = p.lock() {
                    p.current = i;
                }
            }
        }
        let Some((addr, bytes, word)) = text.word_at(i) else { break };

        // Symbol header — if this address starts a named symbol.
        if let Some(sym) = symbols.at(addr) {
            rows.push(ListingRow::SymbolHeader {
                name: SharedString::from(sym.display_name.clone()),
            });
        }

        // Decode + format.
        let decoded = armv8_encode::isa::aarch64::decode_instruction(addr, word).ok();
        let (mnemonic, mut operands, terminates, target_addr) = match &decoded {
            Some(insn) => {
                let m = fmt::mnemonic_chunk(insn).text;
                let ops = fmt::operands_chunks(insn);
                let term = fmt::is_terminator(insn.mnemonic);
                let tgt = fmt::primary_address_operand(insn);
                (m, ops, term, tgt)
            }
            None => (
                ".word".to_string(),
                vec![glass_arch_arm64::Chunk {
                    text: format!("0x{word:08x}"),
                    kind: glass_arch_arm64::ChunkKind::Immediate,
                    target: None,
                    target_text: None,
                }],
                false,
                None,
            ),
        };

        // Resolve any Address chunks (branch/page targets) to a
        // friendlier label in-place. Preference order:
        //   1. Covering symbol *whose section is the same as the
        //      target*. A symbol with non-zero `size` from the
        //      symtab can claim a range that overruns its real
        //      extent — common for unwind tables like
        //      `GCC_except_table0` — and ends up labelling
        //      addresses in completely different sections. Reject
        //      such cross-section "coverage".
        //   2. Containing section → "__sectname+0xoff". For ADRP
        //      operands especially, the operand is a page address
        //      that often has no covering symbol; the section it
        //      lives in is the most honest label we can give.
        //   3. Leave the raw hex as-is.
        // We only mark `named_in_operand` for symbol matches —
        // section labels don't suppress the trailing comment,
        // since the section name alone is less informative than
        // a resolved ADRP+ADD string peek.
        let mut named_in_operand = false;
        for op in &mut operands {
            if op.kind != glass_arch_arm64::ChunkKind::Address {
                continue;
            }
            let Some(t) = op.target else { continue };
            let target_section = data.section_containing(t);
            let symbol_label = symbols.covering(t).and_then(|sym| {
                let sym_section = data.section_containing(sym.address);
                let same_section = match (target_section, sym_section) {
                    (Some((tn, _)), Some((sn, _))) => tn == sn,
                    // If we don't know either section, fall back to
                    // trusting the symbol — better than always
                    // dropping the label.
                    _ => true,
                };
                if !same_section {
                    return None;
                }
                let off = t - sym.address;
                Some(if off == 0 {
                    sym.display_name.clone()
                } else {
                    format!("{}+0x{off:x}", sym.display_name)
                })
            });
            if let Some(label) = symbol_label {
                op.text = label;
                named_in_operand = true;
            } else if let Some((sec_name, sec_base)) = target_section {
                let off = t - sec_base;
                op.text = if off == 0 {
                    sec_name.to_string()
                } else {
                    format!("{sec_name}+0x{off:x}")
                };
            }
        }

        // Comment only when the operand itself doesn't name the
        // target. The operand-substitution above also applies a
        // section-name fallback, so a comment here would just
        // restate the section label — keep this branch for
        // covering-symbol cases only, and apply the same
        // same-section sanity check (a symbol from a different
        // section is treating its size attribute as overrunning).
        let comment = if named_in_operand {
            SharedString::from("")
        } else {
            target_addr
                .and_then(|t| {
                    let sym = symbols.covering(t)?;
                    let target_section = data.section_containing(t);
                    let sym_section = data.section_containing(sym.address);
                    let same_section = match (target_section, sym_section) {
                        (Some((tn, _)), Some((sn, _))) => tn == sn,
                        _ => true,
                    };
                    if !same_section {
                        return None;
                    }
                    let off = t - sym.address;
                    Some(if off == 0 {
                        SharedString::from(format!("; {}", sym.display_name))
                    } else {
                        SharedString::from(format!("; {} + 0x{off:x}", sym.display_name))
                    })
                })
                .unwrap_or_else(|| SharedString::from(""))
        };

        // Pair / direct-address comment. Cases (first match wins):
        //   1. ADD Xd, Xs, #imm  where x_page_bases[Xs] is some(page)
        //      → resolved = page + imm; peek string.
        //   2. ADR Xd, label     → resolved = label; peek string.
        let mut resolved_addr: Option<u64> = None;
        // Source reg of a matched ADD — needed to find the ADRP
        // row for the cross-section retro-label below.
        let mut resolved_via_source: Option<u8> = None;
        if let Some(insn) = decoded.as_ref() {
            if let Some((_d, s, target)) = extract_add_with_imm(insn, &x_page_bases) {
                resolved_addr = Some(target);
                resolved_via_source = Some(s);
            } else if matches!(
                insn.mnemonic,
                armv8_encode::isa::aarch64::Aarch64Mnemonic::Adr
            ) {
                resolved_addr = insn.operands.iter().find_map(|op| match op {
                    armv8_encode::isa::aarch64::DecodedOperand::BranchTarget(a) => {
                        Some(*a)
                    }
                    _ => None,
                });
            }
        }
        let comment = if let Some(addr_for_string) = resolved_addr {
            match data.peek_string(addr_for_string, 64) {
                Some(s) => {
                    let trimmed: String = s.chars().take(64).collect();
                    SharedString::from(format!("; \"{trimmed}\""))
                }
                None => {
                    // Useful while debugging: tell us when we resolved
                    // an adrp/adr target but the bytes there weren't a
                    // printable string. Indicates either a different
                    // pattern (adrp+ldr) or a non-string pointer.
                    tracing::trace!(
                        "adrp/adr resolved to 0x{addr_for_string:x} \
                         (no printable string at target; \
                         data sections cached: {})",
                        data.sections.len()
                    );
                    comment
                }
            }
        } else {
            comment
        };

        rows.push(ListingRow::Instruction {
            address: addr,
            bytes,
            mnemonic: SharedString::from(mnemonic),
            operands: Arc::new(operands),
            comment,
            arrows: Arc::new(Vec::new()),
        });

        // Cross-section retro-label for the matching ADRP. When
        // the resolved ADRP+ADD target sits in a *different*
        // section from the ADRP's page address — the common case
        // where a string lives just past the end of the unwind
        // tables and the linker emitted ADRP at the last page of
        // the previous section — the literal page label
        // (`__gcc_except_tab+0x6300`) is misleading because the
        // user reads the ADRP and naturally cares about where it
        // ends up. Rewrite the ADRP operand as
        // `<dest_section>-0x<imm>` so the reader sees the
        // destination at a glance; the negative offset makes the
        // arithmetic explicit (ADRP + ADD = dest, so ADRP =
        // dest - ADD_imm).
        if let (Some(target), Some(src), Some(insn)) =
            (resolved_addr, resolved_via_source, decoded.as_ref())
        {
            let add_imm = first_imm_of(insn).unwrap_or(0);
            if add_imm > 0 {
                if let Some(row_idx) =
                    x_page_origin_row.get(src as usize).copied().flatten()
                {
                    if let Some(ListingRow::Instruction { operands, .. }) =
                        rows.get_mut(row_idx)
                    {
                        // Resolve target + page → section labels.
                        let target_section = data.section_containing(target);
                        let page = target.saturating_sub(add_imm as u64);
                        let page_section = data.section_containing(page);
                        let cross_section = match (target_section, page_section) {
                            (Some((tn, _)), Some((pn, _))) => tn != pn,
                            _ => false,
                        };
                        if cross_section {
                            if let Some((tn, _)) = target_section {
                                // tweak the ADRP's address chunk
                                // (page) to read "<dest>-0x<imm>".
                                let ops_mut = Arc::make_mut(operands);
                                for op in ops_mut.iter_mut() {
                                    if op.kind
                                        != glass_arch_arm64::ChunkKind::Address
                                    {
                                        continue;
                                    }
                                    op.text = format!("{tn}-0x{add_imm:x}");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Update per-register page-base state.
        //
        //   - ADRP Xd, page  → x_page_bases[d] = page.
        //   - Otherwise, if the instruction writes Xd, invalidate
        //     x_page_bases[d] (a write loses the page base).
        if let Some(insn) = decoded.as_ref() {
            if let Some((d, page)) = extract_adrp(insn) {
                if (d as usize) < x_page_bases.len() {
                    x_page_bases[d as usize] = Some(page);
                    x_page_origin_row[d as usize] = Some(rows.len() - 1);
                }
            } else if let Some(d) = dest_x_reg(insn) {
                if (d as usize) < x_page_bases.len() {
                    x_page_bases[d as usize] = None;
                    x_page_origin_row[d as usize] = None;
                }
            }
        }

        if terminates {
            rows.push(ListingRow::BasicBlockSeparator {
                arrows: Arc::new(Vec::new()),
            });
        }
    }

    assign_arrows(&mut rows);

    if let Some(p) = progress {
        if let Ok(mut p) = p.lock() {
            p.current = n;
            p.done = true;
        }
    }
    rows
}

/// After rows are built, scan every Instruction for a direct branch
/// whose target lies inside the same function and attach `ArrowSegment`s
/// to source / target / passing rows. Functions are delimited by
/// `SymbolHeader` rows (between any two consecutive headers).
///
/// Arrows are assigned lanes by a tiny sweepline so simultaneously-
/// active arrows don't visually merge. Lane 0 is closest to the
/// address column; higher lanes sit further left.
fn assign_arrows(rows: &mut [ListingRow]) {
    use glass_arch_arm64::format as fmt;
    // Build address → row-index lookup, and segment the rows into
    // [start, end) function ranges using SymbolHeader positions.
    let mut addr_to_row: std::collections::HashMap<u64, usize> =
        std::collections::HashMap::with_capacity(rows.len());
    let mut header_rows: Vec<usize> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match r {
            ListingRow::SymbolHeader { .. } => header_rows.push(i),
            ListingRow::Instruction { address, .. } => {
                addr_to_row.insert(*address, i);
            }
            _ => {}
        }
    }
    // Function ranges: [headers[k], headers[k+1]), and the prefix
    // before the first header (if any) and the suffix after the last.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut prev = 0usize;
    for &h in &header_rows {
        if h > prev {
            ranges.push((prev, h));
        }
        prev = h;
    }
    if prev < rows.len() {
        ranges.push((prev, rows.len()));
    }

    // Collect candidate arrows per function.
    #[derive(Clone)]
    struct PendingArrow {
        src_row: usize,
        tgt_row: usize,
        style: ArrowStyle,
    }
    let mut pending: Vec<PendingArrow> = Vec::new();
    for (lo, hi) in &ranges {
        for src_row in *lo..*hi {
            let ListingRow::Instruction { address: _, bytes, .. } = &rows[src_row] else {
                continue;
            };
            let word = u32::from_le_bytes(*bytes);
            // Re-decode to avoid storing the mnemonic on every row.
            // Branches are sparse — cost is negligible.
            let addr_of_row = if let ListingRow::Instruction { address, .. } = &rows[src_row] {
                *address
            } else {
                continue;
            };
            let Ok(insn) = armv8_encode::isa::aarch64::decode_instruction(addr_of_row, word)
            else {
                continue;
            };
            let style = if fmt::is_unconditional_direct_branch(insn.mnemonic) {
                ArrowStyle::Solid
            } else if fmt::is_conditional_branch(insn.mnemonic) {
                ArrowStyle::Dotted
            } else {
                continue;
            };
            let Some(target) = fmt::primary_address_operand(&insn) else { continue };
            let Some(&tgt_row) = addr_to_row.get(&target) else { continue };
            // "Within the function" — both endpoints inside the same
            // [lo, hi) range. Target row must be an Instruction (not a
            // separator) in that span. Since we only inserted
            // Instruction rows into addr_to_row, the second condition
            // is automatic; we just check the range.
            if tgt_row < *lo || tgt_row >= *hi {
                continue;
            }
            if tgt_row == src_row {
                continue;
            }
            pending.push(PendingArrow { src_row, tgt_row, style });
        }
    }

    // Lane assignment: sweepline. Sort by source row, then assign each
    // arrow the lowest lane whose previous occupant has already ended.
    pending.sort_by_key(|a| a.src_row);
    let mut lane_free_at: Vec<usize> = Vec::new(); // lane_free_at[lane] = first row index that lane is free
    for a in &pending {
        let (lo, hi) = if a.src_row <= a.tgt_row {
            (a.src_row, a.tgt_row)
        } else {
            (a.tgt_row, a.src_row)
        };
        // Find a free lane.
        let mut lane = None;
        for (idx, free_at) in lane_free_at.iter_mut().enumerate() {
            if *free_at <= lo {
                lane = Some(idx);
                *free_at = hi + 1;
                break;
            }
        }
        let lane = match lane {
            Some(l) => l,
            None => {
                lane_free_at.push(hi + 1);
                lane_free_at.len() - 1
            }
        };
        // Drop arrows that would overflow the visible gutter rather
        // than draw them clipped or off-screen.
        if (lane as u8) >= ARROW_MAX_LANES {
            continue;
        }
        let dir = if a.src_row < a.tgt_row {
            ArrowDirection::Down
        } else {
            ArrowDirection::Up
        };
        // Emit segments. We mutate `rows[row]` directly — `arrows` is
        // Arc<Vec<_>> so make_mut to clone-on-write into our owned copy.
        let push_seg = |rows: &mut [ListingRow], row: usize, role: ArrowRole| {
            let seg = ArrowSegment {
                lane: lane as u8,
                style: a.style,
                role,
                direction: dir,
            };
            match &mut rows[row] {
                ListingRow::Instruction { arrows, .. } => {
                    Arc::make_mut(arrows).push(seg);
                }
                ListingRow::BasicBlockSeparator { arrows } => {
                    // BB separators only ever host pass-through
                    // segments (the line continues over them). Force
                    // the role so a row that happens to coincide with
                    // a separator still draws a clean vertical.
                    let mut pass = seg;
                    pass.role = ArrowRole::Pass;
                    Arc::make_mut(arrows).push(pass);
                }
                _ => {}
            }
        };
        push_seg(rows, a.src_row, ArrowRole::Source);
        push_seg(rows, a.tgt_row, ArrowRole::Target);
        let (mid_lo, mid_hi) = if a.src_row < a.tgt_row {
            (a.src_row + 1, a.tgt_row)
        } else {
            (a.tgt_row + 1, a.src_row)
        };
        for r in mid_lo..mid_hi {
            push_seg(rows, r, ArrowRole::Pass);
        }
    }
}
pub fn listing_row_for_addr(rows: &[ListingRow], addr: u64) -> Option<usize> {
    // Linear is fine for now — listings are at most ~200k rows. A
    // BTreeMap<address, row_index> would scale better; revisit when
    // we have a binary that struggles.
    let mut best: Option<usize> = None;
    for (i, r) in rows.iter().enumerate() {
        if let ListingRow::Instruction { address, .. } = r {
            if *address <= addr {
                best = Some(i);
            } else {
                break;
            }
        }
    }
    best
}
