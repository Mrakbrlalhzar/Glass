//! CFG basic-block model + per-block renderer.
//!
//! `build_cfg_from_text_sections` looks up the text section covering
//! an entry address and delegates to armv8-encode's bytes-based CFG
//! builder. The block renderers (`render_cfg_block_pill` for low LOD,
//! `render_cfg_block_content` for mid/high) consume a pre-computed
//! `CfgBlockSummary` so the call sites in `render_cfg` can resolve
//! call targets once per block.

use gpui::{div, prelude::*, rgb, App, SharedString};

use crate::palette::{
    COLOUR_ADDR, COLOUR_ADDRESS_OP, COLOUR_BYTES, COLOUR_MNEMONIC, COLOUR_PUNCT, COLOUR_REGISTER,
    COLOUR_SYMBOL_HEADER,
};
use crate::{Shell, TextSectionBytes};

pub const CFG_BLOCK_BORDER: u32 = 0x6b6b78;

/// Build a CFG without holding a full `Container`. We have the
/// per-artifact text-section bytes on `LoadedBundle` (used by the
/// linear-listing builder); look up which text section covers
/// `entry_addr` and delegate to armv8-encode's bytes-based CFG
/// builder.
pub fn build_cfg_from_text_sections(
    text_sections: &std::collections::HashMap<
        (glass_db::ArtifactId, String),
        TextSectionBytes,
    >,
    symbols: &glass_arch_arm64::SymbolMap,
    artifact: &glass_db::ArtifactId,
    entry_addr: u64,
) -> Option<glass_arch_arm64::FunctionCfg> {
    for ((aid, _name), section) in text_sections {
        if aid != artifact {
            continue;
        }
        let end = section.base + section.bytes.len() as u64;
        if entry_addr >= section.base && entry_addr < end {
            return glass_arch_arm64::build_function_cfg_from_bytes(
                section.base,
                &section.bytes,
                symbols,
                entry_addr,
            );
        }
    }
    None
}

/// Background fill for a normal block. Exits (`ret` / outside-fn
/// branches) get a warm tint so they stand out at low zoom.
///
/// `function_tint` is the user's colour annotation on the
/// function as a whole — if Some, it overrides the default with
/// a heavy alpha-dimmed version so the block reads as "in this
/// function" rather than the default neutral grey.
pub fn cfg_block_bg(
    block: &glass_arch_arm64::BasicBlock,
    function_tint: Option<u32>,
) -> gpui::Rgba {
    if let Some(rgba) = function_tint {
        // ~12% alpha for the whole-block tint — even gentler than
        // the listing's per-row alpha because the CFG block is a
        // bigger surface and reads as a flat wash.
        let dimmed = (rgba & 0xffffff00) | 0x20;
        return gpui::rgba(dimmed);
    }
    if block.exits_function {
        gpui::rgba(0x3a2c2cff)
    } else {
        gpui::rgba(0x2a313cff)
    }
}

/// Pre-resolved presentational info for a CFG block. Computed once
/// per block by `render_cfg`; the LOD-specific render fns consume it.
pub struct CfgBlockSummary {
    /// Demangled symbol name when this address starts a named
    /// symbol — typically only the function-entry block.
    pub symbol: Option<SharedString>,
    /// Map from call-site instruction address to the resolved
    /// `(callee_entry_addr, display_name)`.
    pub calls: std::collections::HashMap<u64, (u64, SharedString)>,
}

/// What a CFG block should render given the current pixel budget.
#[derive(Clone, Copy)]
pub struct CfgLayoutPlan {
    /// Number of preview rows shown at the top of the block.
    pub preview: usize,
    /// True when a `… <N> instructions` divider line is shown.
    pub show_ellipsis: bool,
    /// True when the last instruction is shown after the divider.
    pub show_last: bool,
}

/// Render context for CFG block content. Carries the bits the
/// renderer needs to wire call-target clicks back to the shell.
pub struct CfgBlockRenderCtx {
    pub shell: gpui::WeakEntity<Shell>,
    pub artifact: glass_db::ArtifactId,
    /// Block index — used by gpui's id() to keep stateful elements
    /// (per-row click handlers) distinct across blocks.
    pub block_idx: usize,
    /// Snapshot of the per-artifact annotation index used to tint
    /// individual instruction rows + the block background. Cloning
    /// is cheap (all fields are `Arc`).
    pub annotations: Option<crate::AnnotationIndex>,
    /// If the function's entry address (or its covering symbol)
    /// has a colour annotation, every block in the function gets
    /// that tint as its background. Resolved once by `render_cfg`.
    pub function_tint: Option<u32>,
}

pub fn render_cfg_block_pill(
    block: &glass_arch_arm64::BasicBlock,
    summary: &CfgBlockSummary,
    dim: gpui::Rgba,
    function_tint: Option<u32>,
) -> gpui::AnyElement {
    let label = summary
        .symbol
        .clone()
        .unwrap_or_else(|| SharedString::from(format!("{:#x}", block.start_addr)));
    div()
        .size_full()
        .bg(cfg_block_bg(block, function_tint))
        .border_2()
        .border_color(rgb(CFG_BLOCK_BORDER))
        .rounded_sm()
        .flex()
        .items_center()
        .justify_center()
        .text_color(dim)
        .text_xs()
        .font_family("Menlo")
        .child(label)
        .into_any_element()
}

pub fn render_cfg_block_content(
    block: &glass_arch_arm64::BasicBlock,
    summary: &CfgBlockSummary,
    plan: CfgLayoutPlan,
    ctx: Option<&CfgBlockRenderCtx>,
) -> gpui::AnyElement {
    let mut body = div()
        .flex()
        .flex_col()
        .size_full()
        .px_2()
        .py_1()
        .text_xs()
        .font_family("Courier New");

    if let Some(name) = summary.symbol.as_ref() {
        body = body.child(
            div()
                .text_color(rgb(COLOUR_SYMBOL_HEADER))
                .child(SharedString::from(format!("{name}:"))),
        );
    }
    let total = block.instructions.len();
    let annotation_at = |addr: u64| -> Option<glass_db::Annotation> {
        let c = ctx?;
        c.annotations.as_ref()?.at_address(addr).cloned()
    };
    let render_insn = |insn: &glass_arch_arm64::InstructionEntry,
                       insn_idx: usize|
     -> gpui::AnyElement {
        let annotation = annotation_at(insn.address);
        let mut row = div().flex().flex_row().gap_2().whitespace_nowrap();
        // Per-row tint: same dim-alpha treatment as the listing.
        if let Some(rgba) = annotation.as_ref().and_then(|a| a.colour) {
            let dimmed = (rgba & 0xffffff00) | 0x3c;
            row = row.bg(gpui::rgba(dimmed)).rounded_sm();
        }
        row = row.child(
            div()
                .text_color(rgb(COLOUR_ADDR))
                .child(SharedString::from(format!("{:016x}", insn.address))),
        );
        row = row.child(
            div()
                .text_color(rgb(COLOUR_MNEMONIC))
                .child(SharedString::from(insn.mnemonic.clone())),
        );
        let call = summary.calls.get(&insn.address);
        if let Some((entry_addr, name)) = call {
            let entry_addr = *entry_addr;
            let name = name.clone();
            let label = name;
            let elem: gpui::AnyElement = match ctx {
                Some(c) => {
                    let weak = c.shell.clone();
                    let artifact = c.artifact.clone();
                    div()
                        .id((
                            "cfg-call",
                            c.block_idx * 1024 + insn_idx,
                        ))
                        .text_color(rgb(COLOUR_ADDRESS_OP))
                        .cursor_pointer()
                        .hover(|s| s.bg(gpui::rgba(0xffffff20)))
                        .child(label)
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            move |_ev, _w, cx: &mut App| {
                                cx.stop_propagation();
                                if let Some(entity) = weak.upgrade() {
                                    let artifact = artifact.clone();
                                    cx.update_entity(&entity, |shell, cx| {
                                        shell.show_cfg(
                                            artifact,
                                            entry_addr,
                                            SharedString::from(""),
                                            cx,
                                        );
                                    });
                                }
                            },
                        )
                        .into_any_element()
                }
                None => div()
                    .text_color(rgb(COLOUR_ADDRESS_OP))
                    .child(label)
                    .into_any_element(),
            };
            row = row.child(elem);
        } else if !insn.operands.is_empty() {
            row = row.child(
                div()
                    .text_color(rgb(COLOUR_REGISTER))
                    .child(SharedString::from(insn.operands.clone())),
            );
        }
        if let Some(comment) = annotation.as_ref().and_then(|a| a.comment.as_deref()) {
            row = row.child(
                div()
                    .text_color(rgb(crate::palette::COLOUR_COMMENT))
                    .child(SharedString::from(format!("; {comment}"))),
            );
        }
        row.into_any_element()
    };
    for (i, insn) in block.instructions.iter().take(plan.preview).enumerate() {
        body = body.child(render_insn(insn, i));
    }
    if plan.show_ellipsis {
        let first_skipped = plan.preview;
        let last_skipped =
            total.saturating_sub(if plan.show_last { 1 } else { 0 });
        let skipped = last_skipped.saturating_sub(first_skipped);
        // Scan the skipped instructions for any address that has
        // an annotation. If we find one, tint the ellipsis row
        // with its colour so the user knows there's something
        // they should expand to see.
        let elided_colour: Option<u32> = ctx.and_then(|c| {
            let idx = c.annotations.as_ref()?;
            block
                .instructions
                .iter()
                .skip(first_skipped)
                .take(skipped)
                .find_map(|insn| idx.at_address(insn.address).and_then(|a| a.colour))
        });
        let mut ellipsis_row = div().flex().flex_row().gap_2();
        if let Some(rgba) = elided_colour {
            let dimmed = (rgba & 0xffffff00) | 0x3c;
            ellipsis_row = ellipsis_row.bg(gpui::rgba(dimmed)).rounded_sm();
        }
        body = body.child(
            ellipsis_row
                .child(
                    div()
                        .text_color(rgb(COLOUR_PUNCT))
                        .text_lg()
                        .child(SharedString::from("…")),
                )
                .child(
                    div()
                        .text_color(rgb(COLOUR_BYTES))
                        .child(SharedString::from(format!("{skipped} instructions"))),
                ),
        );
    }
    if plan.show_last {
        if let Some(last) = block.instructions.last() {
            body = body.child(render_insn(last, total.saturating_sub(1)));
        }
    }
    if total == 0 {
        body = body.child(
            div()
                .text_color(rgb(COLOUR_BYTES))
                .child(SharedString::from("(empty)")),
        );
    }

    let function_tint = ctx.and_then(|c| c.function_tint);
    div()
        .size_full()
        .bg(cfg_block_bg(block, function_tint))
        .border_2()
        .border_color(rgb(CFG_BLOCK_BORDER))
        .rounded_sm()
        .overflow_hidden()
        .child(body)
        .into_any_element()
}
