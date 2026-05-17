//! CFG basic-block model + per-block renderer.
//!
//! `build_cfg_from_text_sections` looks up the text section covering
//! an entry address and delegates to armv8-encode's bytes-based CFG
//! builder. The block renderers (`render_cfg_block_pill` for low LOD,
//! `render_cfg_block_content` for mid/high) consume a pre-computed
//! `CfgBlockSummary` so the call sites in `render_cfg` can resolve
//! call targets once per block.

use gpui::{div, prelude::*, px, rgb, App, SharedString};

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
pub fn cfg_block_bg(block: &glass_arch_arm64::BasicBlock) -> gpui::Rgba {
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
}

pub fn render_cfg_block_pill(
    block: &glass_arch_arm64::BasicBlock,
    summary: &CfgBlockSummary,
    dim: gpui::Rgba,
) -> gpui::AnyElement {
    let label = summary
        .symbol
        .clone()
        .unwrap_or_else(|| SharedString::from(format!("{:#x}", block.start_addr)));
    div()
        .size_full()
        .bg(cfg_block_bg(block))
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
    let render_insn = |insn: &glass_arch_arm64::InstructionEntry,
                       insn_idx: usize|
     -> gpui::AnyElement {
        let mut row = div().flex().flex_row().gap_2().whitespace_nowrap();
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
            let label = SharedString::from(name);
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
        row.into_any_element()
    };
    for (i, insn) in block.instructions.iter().take(plan.preview).enumerate() {
        body = body.child(render_insn(insn, i));
    }
    if plan.show_ellipsis {
        let skipped = total
            .saturating_sub(plan.preview)
            .saturating_sub(if plan.show_last { 1 } else { 0 });
        body = body.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
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

    div()
        .size_full()
        .bg(cfg_block_bg(block))
        .border_2()
        .border_color(rgb(CFG_BLOCK_BORDER))
        .rounded_sm()
        .overflow_hidden()
        .child(body)
        .into_any_element()
}
