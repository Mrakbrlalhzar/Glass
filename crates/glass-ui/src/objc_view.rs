//! Tab renderer for `TabKind::ObjCClass`.
//!
//! Structurally mirrors the smali viewer's "rendered text +
//! clickable chunks" layout but operates on pre-rendered
//! [`ObjCRow`](glass_arch_arm::objc_format::ObjCRow) values cached
//! on the bundle. The address chunks emitted by `objc_format` use
//! `ChunkKind::Address` with `target = Some(imp_vaddr)`; clicking
//! them resolves the artifact's text section for that address and
//! opens / focuses a Listing tab scrolled to the IMP.

use std::sync::Arc;

use gpui::{
    div, list, prelude::*, px, rgb, App, Context, ListAlignment, ListState, SharedString,
};

use crate::listing_render::LISTING_ROW_MIN_WIDTH;
use crate::palette::{chunk_colour, COLOUR_PLAIN, COLOUR_ROW_SELECTED};
use crate::scrollbar::{horizontal_scrollbar_offset, list_scrollbar};
use crate::{LoadedBundle, Shell, TabKind, TextTooltip};

const ROW_H: f32 = 22.;

pub(crate) fn render_objc_tab(
    shell: &mut Shell,
    bundle: LoadedBundle,
    cx: &mut Context<Shell>,
    border: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
) -> gpui::AnyElement {
    let (artifact, class_name) = match shell
        .active_tab
        .and_then(|i| shell.tabs.get(i))
        .map(|t| t.kind.clone())
    {
        Some(TabKind::ObjCClass { artifact, class_name }) => (artifact, class_name),
        _ => return div().flex_1().into_any_element(),
    };

    let rows: Arc<Vec<glass_arch_arm::objc_format::ObjCRow>> = bundle
        .objc_classes
        .get(&(artifact.clone(), class_name.clone()))
        .cloned()
        .unwrap_or_else(|| Arc::new(Vec::new()));

    let (scroll, h_offset, selected_row) = match shell
        .active_tab
        .and_then(|i| shell.tabs.get(i))
    {
        Some(tab) => (tab.scroll.clone(), tab.h_offset, tab.selected_row),
        None => (
            ListState::new(rows.len(), ListAlignment::Top, px(2000.)),
            px(0.),
            None,
        ),
    };

    let v_scrollbar = list_scrollbar(&scroll, border, dim);
    let h_scrollbar =
        horizontal_scrollbar_offset(h_offset, px(LISTING_ROW_MIN_WIDTH), border, dim);
    let max_h = px(LISTING_ROW_MIN_WIDTH);
    let weak = cx.entity().downgrade();
    let weak_for_scroll = weak.clone();

    div()
        .flex_1()
        .flex()
        .flex_col()
        .min_h_0()
        .child(
            div()
                .flex_1()
                .relative()
                .overflow_hidden()
                .on_scroll_wheel(cx.listener(
                    move |this, ev: &gpui::ScrollWheelEvent, _w, cx| {
                        let dx = ev.delta.pixel_delta(px(ROW_H)).x;
                        if dx != px(0.) {
                            this.scroll_h_by(-dx, max_h, cx);
                        }
                    },
                ))
                .child(
                    list(scroll, {
                        let rows = rows.clone();
                        let artifact = artifact.clone();
                        let bundle = bundle.clone();
                        let weak = weak.clone();
                        let _ = weak_for_scroll;
                        move |index, _window, _cx| {
                            let Some(row) = rows.get(index) else {
                                return div().into_any_element();
                            };
                            render_row(
                                row,
                                index,
                                h_offset,
                                selected_row == Some(index),
                                accent,
                                &artifact,
                                &bundle,
                                &weak,
                            )
                        }
                    })
                    .size_full(),
                )
                .child(v_scrollbar),
        )
        .child(h_scrollbar)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_row(
    row: &glass_arch_arm::objc_format::ObjCRow,
    index: usize,
    h_offset: gpui::Pixels,
    is_selected: bool,
    accent: gpui::Rgba,
    artifact: &glass_db::ArtifactId,
    bundle: &LoadedBundle,
    weak: &gpui::WeakEntity<Shell>,
) -> gpui::AnyElement {
    let mut inner = div()
        .absolute()
        .top_0()
        .left(-h_offset)
        .h(px(ROW_H))
        .w(px(LISTING_ROW_MIN_WIDTH))
        .pl_4()
        .pr_3()
        .text_base()
        .font_family("Courier New")
        .text_color(rgb(COLOUR_PLAIN()))
        .whitespace_nowrap()
        .flex()
        .flex_row()
        .items_center();
    for (i, tok) in row.chunks.iter().enumerate() {
        let base = div()
            .text_color(rgb(chunk_colour(tok.kind)))
            .whitespace_nowrap()
            .child(SharedString::from(tok.text.clone()));
        // Clickable IMP address chunks: resolve the artifact's
        // text section that covers `target` and open the listing
        // tab scrolled to it. Same UX as listing operand clicks
        // (shift = force new tab).
        if tok.kind == glass_arch_arm::ChunkKind::Address {
            if let Some(target) = tok.target {
                let section = bundle
                    .text_section_for_addr(artifact, target)
                    .map(|s| s.to_string());
                if let Some(section_name) = section {
                    let weak = weak.clone();
                    let artifact = artifact.clone();
                    let tooltip_label =
                        format!("Follow {}  (⇧+click = new tab)", tok.text);
                    let chip = base
                        .id(("objc-addr", index * 1024 + i))
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                                let Some(entity) = weak.upgrade() else { return };
                                let artifact = artifact.clone();
                                let section_name = section_name.clone();
                                let new_tab = ev.modifiers.shift;
                                cx.update_entity(&entity, |shell, cx| {
                                    if new_tab {
                                        shell.open_listing_force_new_tab(
                                            artifact,
                                            section_name,
                                            target,
                                            cx,
                                        );
                                    } else {
                                        shell.open_listing_at(
                                            artifact,
                                            section_name,
                                            target,
                                            cx,
                                        );
                                    }
                                });
                                cx.stop_propagation();
                            },
                        )
                        .tooltip(move |_w, cx| {
                            cx.new(|_| TextTooltip {
                                text: SharedString::from(tooltip_label.clone()),
                            })
                            .into()
                        });
                    inner = inner.child(chip);
                    continue;
                }
            }
        }
        inner = inner.child(base);
    }

    let _ = COLOUR_ROW_SELECTED;
    let weak_row = weak.clone();
    let mut shell_row = div()
        .h(px(ROW_H))
        .w_full()
        .overflow_hidden()
        .relative();
    if is_selected {
        shell_row = shell_row.bg(accent);
    }
    shell_row
        .child(inner)
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |_ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                if let Some(entity) = weak_row.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.select_active_row(index, cx);
                    });
                }
            },
        )
        .into_any_element()
}
