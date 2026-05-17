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
use crate::{LoadedBundle, Shell, TextTooltip};

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
    h_shift_inner(inner, h_offset, row_height, row_index, ctx, None, true)
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
    h_shift_inner(inner, h_offset, row_height, row_index, ctx, None, false)
}

pub fn h_shift_with_addr(
    inner: gpui::Div,
    h_offset: Pixels,
    row_height: f32,
    row_index: usize,
    ctx: Option<&RowCtx>,
    row_addr: Option<u64>,
) -> gpui::Stateful<gpui::Div> {
    h_shift_inner(inner, h_offset, row_height, row_index, ctx, row_addr, true)
}

fn h_shift_inner(
    inner: gpui::Div,
    h_offset: Pixels,
    row_height: f32,
    row_index: usize,
    ctx: Option<&RowCtx>,
    row_addr: Option<u64>,
    selectable: bool,
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
    outer.child(
        inner
            .absolute()
            .top_0()
            .left(-h_offset)
            .h(px(row_height))
            .w(px(LISTING_ROW_MIN_WIDTH)),
    )
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
                let make_cell = |w: Pixels, text: String| {
                    let mut c = div()
                        .id(("hex-cell", row_index * 16 + i))
                        .w(w)
                        .whitespace_nowrap()
                        .text_color(rgb(COLOUR_BYTES))
                        .child(text);
                    if is_selected_byte {
                        c = c.bg(rgb(COLOUR_BYTE_SELECTED)).text_color(rgb(0xffffff));
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
            h_shift(inner, h_offset, HEX_ROW_HEIGHT, row_index, ctx)
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
        ListingRow::SymbolHeader { name } => h_shift(
            div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                .child(
                    div().w(px(LISTING_GUTTER_WIDTH)).h_full().flex_shrink_0(),
                )
                .child(
                    div()
                        .text_color(rgb(COLOUR_SYMBOL_HEADER))
                        .child(format!("{name}:")),
                ),
            h_offset,
            LISTING_ROW_HEIGHT,
            row_index,
            ctx,
        ),
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
            if !comment.is_empty() {
                row_div = row_div.child(
                    div()
                        .ml_4()
                        .text_color(rgb(COLOUR_COMMENT))
                        .child(comment.clone()),
                );
            }
            h_shift_with_addr(
                row_div,
                h_offset,
                LISTING_ROW_HEIGHT,
                row_index,
                ctx,
                Some(*address),
            )
        }
    }
}
