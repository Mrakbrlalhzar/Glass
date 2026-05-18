//! Listing + hex row renderers.
//!
//! All renderers here take a `RowCtx` so they can wire row-level
//! click handlers (selection, context menu) and operand-level deep
//! links (jump to address). The rest is layout-only — fixed-width
//! columns matching the listing's column model.

use std::sync::Arc;

use gpui::{div, prelude::*, px, rgb, App, Pixels, SharedString};

use crate::hex::HexRow;
use crate::listing_model::{
    ArrowDirection, ArrowRole, ArrowSegment, ArrowStyle, ListingRow,
};
use crate::palette::{
    chunk_colour, COLOUR_ADDR, COLOUR_BB_SEPARATOR, COLOUR_BYTES, COLOUR_COMMENT,
    COLOUR_MNEMONIC, COLOUR_ROW_SELECTED, COLOUR_SYMBOL_HEADER,
};
use crate::{AnnotationIndex, LoadedBundle, Shell, TextTooltip};

/// Borrow the per-row annotation index, if any. Convenience: avoids
/// repeating the `bundle.annotations.get(&artifact)` dance.
fn annotation_index<'a>(ctx: Option<&'a RowCtx>) -> Option<&'a AnnotationIndex> {
    let ctx = ctx?;
    ctx.bundle.annotations.get(&ctx.artifact)
}

/// Resolve the `[start, end)` byte range of the data item that
/// contains `addr`, when one is determinable. Two heuristics:
///   1. A covering `SymbolKind::Object` with a non-zero size —
///      use `[sym.address, sym.address + sym.size)`.
///   2. The address lives in a "strings" section (name contains
///      `cstring`, `__cfstring`, `__objc_methname`, etc.); scan
///      from `addr` forward to the next NUL and back from `addr`
///      to the previous NUL (or section start) so the highlight
///      covers the whole string the user has selected.
/// Returns `None` when neither heuristic applies — the renderer
/// then draws no item highlight.
fn item_extent_for(ctx: Option<&RowCtx>, addr: u64) -> Option<(u64, u64)> {
    let ctx = ctx?;
    // (1) Symbol-defined data item.
    if let Some(sm) = ctx.bundle.symbol_maps.get(&ctx.artifact) {
        if let Some(sym) = sm.covering(addr) {
            if matches!(sym.kind, glass_arch_arm64::SymbolKind::Object) && sym.size > 0 {
                return Some((sym.address, sym.address + sym.size));
            }
        }
    }
    // (2) String-section NUL scan.
    let section_name = ctx.bundle.data_section_for_addr(&ctx.artifact, addr)?;
    if !looks_like_strings_section(section_name) {
        return None;
    }
    let section = ctx
        .bundle
        .data_sections
        .get(&(ctx.artifact.clone(), section_name.to_string()))?;
    let off = addr.checked_sub(section.base)? as usize;
    if off >= section.bytes.len() {
        return None;
    }
    // Scan back to start of string (previous NUL + 1, or section start).
    let start_off = section.bytes[..off]
        .iter()
        .rposition(|&b| b == 0)
        .map(|p| p + 1)
        .unwrap_or(0);
    // Scan forward to end (next NUL exclusive).
    let end_off = section.bytes[off..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| off + p)
        .unwrap_or(section.bytes.len());
    Some((section.base + start_off as u64, section.base + end_off as u64))
}

fn looks_like_strings_section(name: &str) -> bool {
    // Mach-O: __cstring, __cfstring (CoreFoundation), __objc_methname,
    // __objc_classname, __objc_methtype, __ustring. ELF: .rodata.str*,
    // .strtab, .dynstr, .gnu.linkonce.r.str*. Substring match is fine
    // since we're advising a heuristic, not enforcing.
    let l = name.to_ascii_lowercase();
    l.contains("cstring")
        || l.contains("objc_methname")
        || l.contains("objc_classname")
        || l.contains("objc_methtype")
        || l.contains(".str")
        || l.contains("ustring")
        || l.contains("__gcc_except_tab")
}

pub const LISTING_ROW_HEIGHT: f32 = 22.;
pub const BB_SEPARATOR_HEIGHT: f32 = 8.;
pub const LISTING_GUTTER_WIDTH: f32 = 56.;
/// 16 hex chars + a couple of px of slack. Dropped the `0x` prefix —
/// the column is exclusively addresses, so the marker is redundant and
/// the saved width keeps the address from wrapping inside Courier.
pub const LISTING_ADDR_WIDTH: f32 = 170.;
/// 4 bytes shown as "XX XX XX XX" (11 chars) plus generous padding.
pub const LISTING_BYTES_WIDTH: f32 = 140.;
pub const LISTING_MNEMONIC_WIDTH: f32 = 80.;
/// Min row width so long operand+comment lines have somewhere to slide
/// under a horizontal scroll.
pub const LISTING_ROW_MIN_WIDTH: f32 = 2400.;

const ARROW_LANE_SPACING: f32 = 8.;
/// Distance from the gutter's right edge (= address column) to lane 0.
const ARROW_LANE_RIGHT_MARGIN: f32 = 12.;
const ARROW_THICKNESS: f32 = 2.;
const ARROW_HEAD_LEN: f32 = 6.;
const ARROW_HEAD_HALF: f32 = 4.;

pub const HEX_ROW_HEIGHT: f32 = 22.;
pub const HEX_CELL_WIDTH: f32 = 26.;
pub const HEX_BYTES_WIDTH: f32 = 16.0 * HEX_CELL_WIDTH + 8.;
pub const HEX_ASCII_WIDTH: f32 = 160.;
pub const HEX_ROW_MIN_WIDTH: f32 = 2400.;
const COLOUR_BYTE_SELECTED: u32 = 0x4f7cff;

/// Context passed into a single row's render — needed so Address
/// chunks can wire click-to-goto handlers and rows can mark themselves
/// as selected.
#[derive(Clone)]
pub struct RowCtx {
    pub bundle: LoadedBundle,
    pub artifact: glass_db::ArtifactId,
    pub shell: gpui::WeakEntity<Shell>,
    pub selected_row: Option<usize>,
}

fn lane_x(lane: u8) -> f32 {
    LISTING_GUTTER_WIDTH - ARROW_LANE_RIGHT_MARGIN - (lane as f32) * ARROW_LANE_SPACING
}

pub fn h_shift(
    inner: gpui::Div,
    h_offset: Pixels,
    row_height: f32,
    row_index: usize,
    ctx: Option<&RowCtx>,
) -> gpui::Stateful<gpui::Div> {
    h_shift_inner(inner, h_offset, row_height, row_index, ctx, None, true, None, None)
}

/// Like `h_shift` but the row is non-selectable (no click handler,
/// no selection background). Used for basic-block separators which
/// have no meaningful selection target.
pub fn h_shift_unselectable(
    inner: gpui::Div,
    h_offset: Pixels,
    row_height: f32,
    row_index: usize,
    ctx: Option<&RowCtx>,
) -> gpui::Stateful<gpui::Div> {
    h_shift_inner(inner, h_offset, row_height, row_index, ctx, None, false, None, None)
}

/// Variant that also tints the row background with a user-set
/// RGBA colour. The alpha is dimmed to ~24% so the tint reads as
/// a row highlight without overwhelming the syntax-coloured text.
/// Pass `dot_rgba == Some(_)` to also pin a small dot to the row's
/// right edge — anchored to the clipped outer container so the
/// dot stays visible regardless of horizontal scroll position.
pub fn h_shift_with_addr_annotated(
    inner: gpui::Div,
    h_offset: Pixels,
    row_height: f32,
    row_index: usize,
    ctx: Option<&RowCtx>,
    row_addr: Option<u64>,
    tint_rgba: Option<u32>,
    dot_rgba: Option<u32>,
) -> gpui::Stateful<gpui::Div> {
    h_shift_inner(inner, h_offset, row_height, row_index, ctx, row_addr, true, tint_rgba, dot_rgba)
}

pub(crate) fn h_shift_inner(
    inner: gpui::Div,
    h_offset: Pixels,
    row_height: f32,
    row_index: usize,
    ctx: Option<&RowCtx>,
    row_addr: Option<u64>,
    selectable: bool,
    tint_rgba: Option<u32>,
    dot_rgba: Option<u32>,
) -> gpui::Stateful<gpui::Div> {
    let is_selected = selectable
        && ctx.map(|c| c.selected_row == Some(row_index)).unwrap_or(false);
    let mut outer = div()
        .id(("listing-row", row_index))
        .h(px(row_height))
        .w_full()
        .overflow_hidden()
        .relative();
    if is_selected {
        outer = outer.bg(rgb(COLOUR_ROW_SELECTED));
    } else if let Some(rgba) = tint_rgba {
        // Dim alpha to ~24% so the tint reads as a row highlight
        // rather than overwhelming the syntax-coloured text. The
        // low byte of `rgba` is alpha — we replace it.
        let dimmed = (rgba & 0xffffff00) | 0x3c;
        outer = outer.bg(gpui::rgba(dimmed));
    }
    if selectable {
        if let Some(ctx) = ctx {
            let weak = ctx.shell.clone();
            outer = outer.on_mouse_down(
                gpui::MouseButton::Left,
                move |_ev, _w, cx: &mut App| {
                    if let Some(entity) = weak.upgrade() {
                        cx.update_entity(&entity, |shell, cx| {
                            shell.select_active_row(row_index, cx);
                        });
                    }
                },
            );
            if let Some(addr) = row_addr {
                let weak = ctx.shell.clone();
                let artifact = ctx.artifact.clone();
                outer = outer.on_mouse_down(
                    gpui::MouseButton::Right,
                    move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                        if let Some(entity) = weak.upgrade() {
                            let pos = ev.position;
                            let artifact = artifact.clone();
                            cx.update_entity(&entity, |shell, cx| {
                                shell.open_listing_context_menu(artifact, addr, pos, cx);
                            });
                        }
                    },
                );
            }
        }
    }
    let outer = outer.child(
        inner
            .absolute()
            .top_0()
            .left(-h_offset)
            .h(px(row_height))
            .w(px(LISTING_ROW_MIN_WIDTH)),
    );
    // Edge-dot indicator pinned to the right of the *clipped*
    // outer container so it stays visible regardless of how far
    // the user has scrolled the listing horizontally.
    if let Some(rgba) = dot_rgba {
        outer.child(
            div()
                .absolute()
                .top(px((row_height - 8.) / 2.))
                .right(px(8.))
                .w(px(8.))
                .h(px(8.))
                .rounded_full()
                .bg(gpui::rgba(rgba)),
        )
    } else {
        outer
    }
}

fn render_arrow_gutter(arrows: &Arc<Vec<ArrowSegment>>, row_h: f32) -> gpui::Div {
    let mut gutter = div()
        .w(px(LISTING_GUTTER_WIDTH))
        .h_full()
        .flex_shrink_0()
        .relative();
    if arrows.is_empty() {
        return gutter;
    }
    let mid = (row_h / 2.).floor();
    let colour_solid = gpui::rgba(0x676770ee);
    let colour_dotted = gpui::rgba(0x67677088);
    for seg in arrows.iter() {
        let col = match seg.style {
            ArrowStyle::Solid => colour_solid,
            ArrowStyle::Dotted => colour_dotted,
        };
        let x = lane_x(seg.lane);
        let (v_top, v_height) = match seg.role {
            ArrowRole::Pass => (0., row_h),
            ArrowRole::Source => match seg.direction {
                ArrowDirection::Down => (mid, row_h - mid),
                ArrowDirection::Up => (0., mid),
            },
            ArrowRole::Target => match seg.direction {
                ArrowDirection::Down => (0., mid),
                ArrowDirection::Up => (mid, row_h - mid),
            },
        };
        gutter = gutter.child(
            div()
                .absolute()
                .left(px(x))
                .top(px(v_top))
                .w(px(ARROW_THICKNESS))
                .h(px(v_height))
                .bg(col),
        );
        if matches!(seg.role, ArrowRole::Source | ArrowRole::Target) {
            let stub_end = match seg.role {
                ArrowRole::Target => LISTING_GUTTER_WIDTH - ARROW_HEAD_LEN,
                _ => LISTING_GUTTER_WIDTH,
            };
            gutter = gutter.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(mid - ARROW_THICKNESS / 2.))
                    .w(px(stub_end - x))
                    .h(px(ARROW_THICKNESS))
                    .bg(col),
            );
            if matches!(seg.role, ArrowRole::Target) {
                let base_x = LISTING_GUTTER_WIDTH - ARROW_HEAD_LEN;
                let half = ARROW_HEAD_HALF as i32;
                for dy in -half..=half {
                    let abs_dy = dy.unsigned_abs() as f32;
                    let bar_w =
                        ARROW_HEAD_LEN * (1.0 - abs_dy / (half as f32));
                    if bar_w <= 0. {
                        continue;
                    }
                    let bar_top = mid + dy as f32 - 0.5;
                    gutter = gutter.child(
                        div()
                            .absolute()
                            .left(px(base_x))
                            .top(px(bar_top))
                            .w(px(bar_w))
                            .h(px(1.))
                            .bg(col),
                    );
                }
            }
        }
    }
    gutter
}

pub fn render_hex_row(
    row: &HexRow,
    row_index: usize,
    h_offset: Pixels,
    ctx: Option<&RowCtx>,
    selected_byte_addr: Option<u64>,
) -> gpui::Stateful<gpui::Div> {
    let row_div = match row {
        HexRow::SymbolHeader { name } => h_shift(
            div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                .child(div().w(px(LISTING_GUTTER_WIDTH)).h_full().flex_shrink_0())
                .child(
                    div()
                        .text_color(rgb(COLOUR_SYMBOL_HEADER))
                        .child(format!("{name}:")),
                ),
            h_offset,
            HEX_ROW_HEIGHT,
            row_index,
            ctx,
        ),
        HexRow::Bytes { address, bytes } => {
            // Scan the 16 byte addresses for any annotation so we
            // can tint the row + show the edge dot. The first
            // matching annotation wins (sorted by ascending byte
            // index within the row).
            let row_annotation = annotation_index(ctx).and_then(|idx| {
                (0..16).find_map(|i| {
                    let byte_addr = address + i as u64;
                    idx.at_address(byte_addr).map(|a| (byte_addr, a))
                })
            });
            // Item-extent highlight: when a byte is selected and
            // we can determine the bounds of the data item it
            // belongs to (via symbol size or a NUL scan), paint
            // the cells inside that range with a subtle accent
            // in both the hex and ASCII columns. Lets the user
            // see at a glance how long e.g. a C string is.
            let item_extent: Option<(u64, u64)> =
                selected_byte_addr.and_then(|sel| item_extent_for(ctx, sel));
            let mut hex_cells = div()
                .w(px(HEX_BYTES_WIDTH))
                .flex_shrink_0()
                .flex()
                .flex_row()
                .pr_2();
            let mut ascii_cells = div()
                .w(px(HEX_ASCII_WIDTH))
                .flex_shrink_0()
                .flex()
                .flex_row();
            for i in 0..16 {
                let byte = bytes.get(i).copied();
                let cell_addr = address + i as u64;
                let is_selected_byte = selected_byte_addr == Some(cell_addr);
                let hex_text = match byte {
                    Some(b) => format!("{b:02x}"),
                    None => "  ".to_string(),
                };
                let ascii_glyph = match byte {
                    Some(b) if (0x20..=0x7e).contains(&b) => (b as char).to_string(),
                    Some(_) => ".".to_string(),
                    None => " ".to_string(),
                };
                let in_item = item_extent
                    .map(|(lo, hi)| cell_addr >= lo && cell_addr < hi)
                    .unwrap_or(false);
                let make_cell = |w: Pixels, text: String| {
                    let mut c = div()
                        .id(("hex-cell", row_index * 16 + i))
                        .w(w)
                        .whitespace_nowrap()
                        .text_color(rgb(COLOUR_BYTES))
                        .child(text);
                    if is_selected_byte {
                        c = c.bg(rgb(COLOUR_BYTE_SELECTED)).text_color(rgb(0xffffff));
                    } else if in_item {
                        // ~14% white wash so the item span reads as
                        // a subtle highlight rather than dominant.
                        c = c.bg(gpui::rgba(0xffffff24));
                    }
                    if let Some(ctx) = ctx {
                        if byte.is_some() {
                            let weak = ctx.shell.clone();
                            c = c.cursor_pointer().on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_ev, _w, cx: &mut App| {
                                    if let Some(entity) = weak.upgrade() {
                                        cx.update_entity(&entity, |shell, cx| {
                                            shell.select_active_row(row_index, cx);
                                            shell.select_byte(cell_addr, cx);
                                        });
                                    }
                                    cx.stop_propagation();
                                },
                            );
                        }
                    }
                    c
                };
                hex_cells = hex_cells.child(make_cell(px(HEX_CELL_WIDTH), hex_text));
                ascii_cells = ascii_cells.child(make_cell(px(10.), ascii_glyph));
            }

            let inner = div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                .child(div().w(px(LISTING_GUTTER_WIDTH)).h_full().flex_shrink_0())
                .child(
                    div()
                        .w(px(LISTING_ADDR_WIDTH))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .pr_4()
                        .text_color(rgb(COLOUR_ADDR))
                        .child(format!("{address:016x}")),
                )
                .child(hex_cells)
                .child(ascii_cells);
            // Right-click on the row → annotation menu for the
            // currently-selected byte, falling back to the row's
            // leftmost byte. Tint + edge dot reflect whichever
            // byte in the row has an annotation (first match).
            let click_addr = selected_byte_addr
                .filter(|a| *a >= *address && *a < *address + 16)
                .unwrap_or(*address);
            let tint_rgba = row_annotation.and_then(|(_, a)| a.colour);
            let dot_rgba = if let Some((_, ann)) = row_annotation {
                Some(ann.colour.unwrap_or(0x4f7cffff))
            } else {
                None
            };
            h_shift_with_addr_annotated(
                inner,
                h_offset,
                HEX_ROW_HEIGHT,
                row_index,
                ctx,
                Some(click_addr),
                tint_rgba,
                dot_rgba,
            )
        }
    };
    row_div
}

pub fn render_listing_row_with(
    row: &ListingRow,
    row_index: usize,
    h_offset: Pixels,
    ctx: Option<&RowCtx>,
) -> gpui::Stateful<gpui::Div> {
    match row {
        ListingRow::SymbolHeader { name } => {
            // SymbolHeader rows only show Symbol-keyed annotations
            // (typically just a rename — those are set via MCP or
            // `glass set-rename --key-kind symbol` and persist
            // across symbol-map rebuilds). Address-keyed
            // annotations show on the actual instruction row at
            // the symbol's entry address instead — duplicating
            // them on the header just gave us a stacked tint +
            // double edge dot for the same logical entry.
            let symbol_annot = annotation_index(ctx).and_then(|idx| idx.at_symbol(name));
            let merged_rename = symbol_annot.and_then(|a| a.rename.as_deref());
            let merged_comment = symbol_annot.and_then(|a| a.comment.as_deref());
            let merged_colour = symbol_annot.and_then(|a| a.colour);
            let has_any = merged_rename.is_some()
                || merged_comment.is_some()
                || merged_colour.is_some();
            let renamed = merged_rename;
            let mut inner = div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                .child(
                    div().w(px(LISTING_GUTTER_WIDTH)).h_full().flex_shrink_0(),
                )
                .child(if let Some(new_name) = renamed {
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            div()
                                .italic()
                                .text_color(rgb(COLOUR_SYMBOL_HEADER))
                                .child(format!("{new_name}:")),
                        )
                        .child(
                            div()
                                .text_color(rgb(COLOUR_COMMENT))
                                .child(format!("({name})")),
                        )
                } else {
                    div()
                        .text_color(rgb(COLOUR_SYMBOL_HEADER))
                        .child(format!("{name}:"))
                });
            if let Some(comment) = merged_comment {
                inner = inner.child(
                    div()
                        .ml_4()
                        .text_color(rgb(COLOUR_COMMENT))
                        .child(SharedString::from(format!("; {comment}"))),
                );
            }
            let dot_rgba = if has_any {
                Some(merged_colour.unwrap_or(0x4f7cffff))
            } else {
                None
            };
            // Right-click on the symbol header should open the
            // annotation menu for the symbol's entry address —
            // resolved through the artifact's symbol map. Without
            // this, the SymbolHeader row had no addr → no right-
            // click handler, and clicks fell through to whatever
            // was painted underneath.
            let sym_addr = ctx.and_then(|c| {
                let sm = c.bundle.symbol_maps.get(&c.artifact)?;
                sm.iter().find(|s| s.display_name == *name).map(|s| s.address)
            });
            h_shift_inner(
                inner,
                h_offset,
                LISTING_ROW_HEIGHT,
                row_index,
                ctx,
                sym_addr,
                true,
                merged_colour,
                dot_rgba,
            )
        }
        ListingRow::BasicBlockSeparator { arrows } => h_shift_unselectable(
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(render_arrow_gutter(arrows, BB_SEPARATOR_HEIGHT))
                .child(
                    div()
                        .flex_1()
                        .h(px(1.))
                        .bg(rgb(COLOUR_BB_SEPARATOR)),
                ),
            h_offset,
            BB_SEPARATOR_HEIGHT,
            row_index,
            ctx,
        ),
        ListingRow::Instruction {
            address,
            bytes,
            mnemonic,
            operands,
            comment,
            arrows,
        } => {
            let mut row_div = div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                .child(render_arrow_gutter(arrows, LISTING_ROW_HEIGHT))
                .child(
                    div()
                        .w(px(LISTING_ADDR_WIDTH))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .pr_4()
                        .text_color(rgb(COLOUR_ADDR))
                        .child(format!("{address:016x}")),
                )
                .child(
                    div()
                        .w(px(LISTING_BYTES_WIDTH))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .pr_4()
                        .text_color(rgb(COLOUR_BYTES))
                        .child(format!(
                            "{:02x} {:02x} {:02x} {:02x}",
                            bytes[0], bytes[1], bytes[2], bytes[3]
                        )),
                )
                .child(
                    div()
                        .w(px(LISTING_MNEMONIC_WIDTH))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .text_color(rgb(COLOUR_MNEMONIC))
                        .child(mnemonic.clone()),
                );
            let mut ops_row = div().flex().flex_row().flex_shrink_0();
            for (i, chunk) in operands.iter().enumerate() {
                let base = div()
                    .id(("addr-chunk", i))
                    .text_color(rgb(chunk_colour(chunk.kind)))
                    .child(SharedString::from(chunk.text.clone()));
                let cell: gpui::AnyElement = match (chunk.kind, chunk.target, ctx) {
                    (glass_arch_arm64::ChunkKind::Address, Some(t), Some(ctx)) => {
                        let weak = ctx.shell.clone();
                        let artifact = ctx.artifact.clone();
                        let target = ctx
                            .bundle
                            .text_section_for_addr(&ctx.artifact, t)
                            .map(|s| (s.to_string(), false))
                            .or_else(|| {
                                ctx.bundle
                                    .data_section_for_addr(&ctx.artifact, t)
                                    .map(|s| (s.to_string(), true))
                            });
                        let display = chunk.text.clone();
                        let tooltip_label =
                            format!("Follow {display}  (⇧+click = new tab)");
                        let mut el = base
                            .cursor_pointer()
                            .hover(|this| this.underline());
                        if let Some((section_name, is_data)) = target {
                            // Left-click: shift = force new tab,
                            // plain = reuse same-type tab.
                            let left_weak = weak.clone();
                            let left_artifact = artifact.clone();
                            let left_section = section_name.clone();
                            el = el.on_mouse_down(
                                gpui::MouseButton::Left,
                                move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                                    let Some(entity) = left_weak.upgrade() else {
                                        return;
                                    };
                                    let artifact = left_artifact.clone();
                                    let section_name = left_section.clone();
                                    let new_tab = ev.modifiers.shift;
                                    cx.update_entity(&entity, |shell, cx| {
                                        if is_data {
                                            if new_tab {
                                                shell.open_hex_force_new_tab(
                                                    artifact, section_name, t, cx,
                                                );
                                            } else {
                                                shell.open_hex_in_new_tab(
                                                    artifact, section_name, t, cx,
                                                );
                                            }
                                        } else if new_tab {
                                            shell.open_listing_force_new_tab(
                                                artifact, section_name, t, cx,
                                            );
                                        } else {
                                            shell.open_listing_at(
                                                artifact, section_name, t, cx,
                                            );
                                        }
                                    });
                                    cx.stop_propagation();
                                },
                            );
                            // Right-click: open the link context menu
                            // with Follow / Follow in new tab, plus
                            // Show CFG when the target is in a text
                            // section.
                            let right_weak = weak.clone();
                            let right_artifact = artifact.clone();
                            let right_section = section_name.clone();
                            let right_display = display.clone();
                            el = el.on_mouse_down(
                                gpui::MouseButton::Right,
                                move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                                    let Some(entity) = right_weak.upgrade() else {
                                        return;
                                    };
                                    let artifact = right_artifact.clone();
                                    let section_name = right_section.clone();
                                    let pos = ev.position;
                                    let display = right_display.clone();
                                    cx.update_entity(&entity, |shell, cx| {
                                        shell.open_link_context_menu(
                                            artifact,
                                            section_name,
                                            t,
                                            is_data,
                                            display,
                                            pos,
                                            cx,
                                        );
                                    });
                                    cx.stop_propagation();
                                },
                            );
                        }
                        el.tooltip(move |_window, cx| {
                            cx.new(|_| TextTooltip {
                                text: SharedString::from(tooltip_label.clone()),
                            })
                            .into()
                        })
                        .into_any_element()
                    }
                    _ => base.into_any_element(),
                };
                ops_row = ops_row.child(cell);
            }
            row_div = row_div.child(ops_row);
            // Per-row annotation: looked up by address only.
            // Symbol-keyed annotations apply to the function's
            // SymbolHeader row, not to every instruction.
            let annotation = annotation_index(ctx).and_then(|idx| idx.at_address(*address));
            let user_comment = annotation.and_then(|a| a.comment.as_deref());
            let combined_comment = match (comment.is_empty(), user_comment) {
                (true, Some(uc)) => Some(SharedString::from(format!("; {uc}"))),
                (false, Some(uc)) => Some(SharedString::from(format!("{comment}  ; {uc}"))),
                (false, None) => Some(comment.clone()),
                (true, None) => None,
            };
            if let Some(c) = combined_comment {
                row_div = row_div.child(
                    div()
                        .ml_4()
                        .text_color(rgb(COLOUR_COMMENT))
                        .child(c),
                );
            }
            // Edge dot is placed on the *clipped* outer container
            // by h_shift_with_addr_annotated so it stays visible
            // regardless of horizontal scroll. Default colour is
            // accent-blue when only rename/comment is set.
            let dot_rgba = if annotation.is_some() {
                Some(annotation.and_then(|a| a.colour).unwrap_or(0x4f7cffff))
            } else {
                None
            };
            h_shift_with_addr_annotated(
                row_div,
                h_offset,
                LISTING_ROW_HEIGHT,
                row_index,
                ctx,
                Some(*address),
                annotation.and_then(|a| a.colour),
                dot_rgba,
            )
        }
    }
}
