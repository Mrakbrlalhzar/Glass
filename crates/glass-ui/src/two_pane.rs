//! Two-pane layout: tree on the left, body content on the right.
//!
//! Extracted from `Shell::render_two_pane` as a free function taking
//! `&mut Shell`. The body dispatches on the active tab kind to the
//! per-view renderers (listing, hex, smali, manifest, section_map,
//! cfg, dex_callgraph).

use std::sync::Arc;

use gpui::{
    div, list, prelude::*, px, rgb, App, Context, ListAlignment, ListState, SharedString,
};

use crate::listing_render::{
    render_hex_row, render_listing_row_with, RowCtx, HEX_ROW_HEIGHT, HEX_ROW_MIN_WIDTH,
    LISTING_ROW_HEIGHT, LISTING_ROW_MIN_WIDTH,
};
use crate::palette::{
    chunk_colour, COLOUR_PLAIN, COLOUR_ROW_SELECTED, COLOUR_TYPE, COLOUR_TYPE_EXTERNAL,
};
use crate::scrollbar::{horizontal_scrollbar_offset, list_scrollbar};
use crate::smali::{extract_class_jni, tokenize_smali_line};
use crate::search::jni_to_dotted;
use crate::{
    flatten, LeafId, LoadedBundle, RowAction, RowKind, Shell, TabKind, TextTooltip, VisibleRow,
};

pub fn render_two_pane(
    shell: &mut Shell,
    bundle: LoadedBundle,
    cx: &mut Context<Shell>,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
) -> gpui::AnyElement {
        let rows: Arc<[VisibleRow]> = flatten(&bundle.tree, &shell.expanded).into();
        let selected = shell.active_leaf();
        let self_handle = cx.entity().downgrade();

        let left_scrollbar = list_scrollbar(&shell.list_state, border, dim);
        let left = div()
            .w(px(340.))
            .h_full()
            .flex_shrink_0()
            .relative()
            .border_r_1()
            .border_color(border)
            .bg(panel)
            .child(
                div().size_full().flex().flex_col().child(
                list(shell.list_state.clone(), {
                    let rows = rows.clone();
                    move |index, _window, _cx| {
                        let row = rows[index].clone();
                        let handle = self_handle.clone();
                        let indent = px(8. + row.depth as f32 * 14.);

                        let (is_selected, glyph, label, on_click_kind): (bool, &'static str, SharedString, RowAction) = match row.kind {
                            RowKind::Group { ref path, expanded, ref label } => (
                                false,
                                if expanded { "▾ " } else { "▸ " },
                                label.clone(),
                                RowAction::Toggle(path.clone()),
                            ),
                            RowKind::Leaf { leaf_id, ref label } => (
                                selected == Some(leaf_id),
                                "  ",
                                label.clone(),
                                RowAction::Select(leaf_id),
                            ),
                        };

                        let row_bg = if is_selected { accent } else { panel };
                        let row_fg = if is_selected { rgb(0xffffff) } else { fg };

                        div()
                            .h(px(22.))
                            .w_full()
                            .pl(indent)
                            .pr_3()
                            .flex()
                            .items_center()
                            .text_xs()
                            .bg(row_bg)
                            .text_color(row_fg)
                            .child(format!("{glyph}{label}"))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_event, _window, cx: &mut App| {
                                    let Some(entity) = handle.upgrade() else { return };
                                    let action = on_click_kind.clone();
                                    cx.update_entity(&entity, |shell, cx| match action {
                                        RowAction::Toggle(path) => shell.toggle_group(path, cx),
                                        RowAction::Select(id) => shell.open_leaf(id, cx),
                                    });
                                },
                            )
                            .into_any()
                    }
                })
                .flex_1(),
                ),
            )
            .child(left_scrollbar);

        shell.ensure_active_tab_lines(cx);
        let (tab_bar, overflow_dropdown) =
            shell.render_tab_bar(&bundle, cx, panel, border, fg, dim, accent);

        let active_kind = shell
            .active_tab
            .and_then(|i| shell.tabs.get(i))
            .map(|t| t.kind.clone());

        let body: gpui::AnyElement = match active_kind {
            Some(TabKind::SectionMap { artifact }) => shell
                .render_section_map(&bundle, &artifact, panel, border, fg, dim, cx)
                .into_any_element(),
            Some(TabKind::Manifest) => {
                let (scroll, h_offset) = match shell
                    .active_tab
                    .and_then(|i| shell.tabs.get(i))
                {
                    Some(tab) => (tab.scroll.clone(), tab.h_offset),
                    None => (
                        ListState::new(0, ListAlignment::Top, px(2000.)),
                        px(0.),
                    ),
                };
                let v_scrollbar = list_scrollbar(&scroll, border, dim);
                let h_scrollbar = horizontal_scrollbar_offset(
                    h_offset,
                    px(LISTING_ROW_MIN_WIDTH),
                    border,
                    dim,
                );
                let max_h = px(LISTING_ROW_MIN_WIDTH);
                let rows = bundle.manifest_rows.clone();
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
                                    let dx = ev.delta.pixel_delta(px(22.)).x;
                                    if dx != px(0.) {
                                        this.scroll_h_by(-dx, max_h, cx);
                                    }
                                },
                            ))
                            .child(
                                list(scroll, move |index, _window, _cx| {
                                    let Some(row) = rows.get(index) else {
                                        return div().into_any();
                                    };
                                    let indent = px(8. + row.depth as f32 * 18.);
                                    // Outer row clips; inner gets translated
                                    // by h_offset so long lines slide left.
                                    let mut inner = div()
                                        .absolute()
                                        .top_0()
                                        .left(-h_offset)
                                        .h(px(22.))
                                        .w(px(LISTING_ROW_MIN_WIDTH))
                                        .pl(indent)
                                        .pr_3()
                                        .text_base()
                                        .font_family("Courier New")
                                        .text_color(rgb(COLOUR_PLAIN))
                                        .whitespace_nowrap()
                                        .flex()
                                        .flex_row()
                                        .items_center();
                                    for tok in row.chunks.iter() {
                                        inner = inner.child(
                                            div()
                                                .text_color(rgb(chunk_colour(tok.kind)))
                                                .whitespace_nowrap()
                                                .child(SharedString::from(tok.text.clone())),
                                        );
                                    }
                                    div()
                                        .h(px(22.))
                                        .w_full()
                                        .overflow_hidden()
                                        .relative()
                                        .child(inner)
                                        .into_any()
                                })
                                .size_full(),
                            )
                            .child(v_scrollbar),
                    )
                    .child(h_scrollbar)
                    .into_any_element()
            }
            Some(TabKind::Hex { artifact, .. }) => {
                let (rows_opt, scroll_opt, h_offset, selected_row, selected_byte) =
                    match shell.active_tab.and_then(|i| shell.tabs.get(i)) {
                        Some(tab) => (
                            tab.hex_rows.clone(),
                            Some(tab.scroll.clone()),
                            tab.h_offset,
                            tab.selected_row,
                            tab.selected_byte_addr,
                        ),
                        None => (None, None, px(0.), None, None),
                    };
                let scroll = scroll_opt.unwrap_or_else(|| {
                    ListState::new(0, ListAlignment::Top, px(2000.))
                });
                let v_scrollbar = list_scrollbar(&scroll, border, dim);
                let h_scrollbar = horizontal_scrollbar_offset(
                    h_offset,
                    px(HEX_ROW_MIN_WIDTH),
                    border,
                    dim,
                );
                let max_h = px(HEX_ROW_MIN_WIDTH);
                let rows = rows_opt.unwrap_or_else(|| Arc::new(Vec::new()));
                let ctx = RowCtx {
                    bundle: bundle.clone(),
                    artifact: artifact.clone(),
                    shell: cx.entity().downgrade(),
                    selected_row,
                };
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
                                    let dx = ev.delta.pixel_delta(px(HEX_ROW_HEIGHT)).x;
                                    if dx != px(0.) {
                                        this.scroll_h_by(-dx, max_h, cx);
                                    }
                                },
                            ))
                            .child(
                                list(scroll, move |index, _window, _cx| {
                                    let Some(row) = rows.get(index) else {
                                        return div().into_any();
                                    };
                                    render_hex_row(
                                        row,
                                        index,
                                        h_offset,
                                        Some(&ctx),
                                        selected_byte,
                                    )
                                    .into_any()
                                })
                                .size_full(),
                            )
                            .child(v_scrollbar),
                    )
                    .child(h_scrollbar)
                    .into_any_element()
            }
            Some(TabKind::Listing { artifact, .. }) => {
                let tab_view = shell.active_tab.and_then(|i| shell.tabs.get(i));
                let (rows_opt, progress_opt, scroll_opt, h_offset, selected_row) =
                    match tab_view {
                        Some(tab) => (
                            tab.listing_rows.clone(),
                            tab.listing_progress.clone(),
                            Some(tab.scroll.clone()),
                            tab.h_offset,
                            tab.selected_row,
                        ),
                        None => (None, None, None, px(0.), None),
                    };
                match (rows_opt, progress_opt) {
                    (Some(listing_rows), _) => {
                        let scroll = scroll_opt.unwrap_or_else(|| {
                            ListState::new(0, ListAlignment::Top, px(2000.))
                        });
                        let v_scrollbar = list_scrollbar(&scroll, border, dim);
                        let h_scrollbar = horizontal_scrollbar_offset(
                            h_offset,
                            px(LISTING_ROW_MIN_WIDTH),
                            border,
                            dim,
                        );
                        let max_h = (px(LISTING_ROW_MIN_WIDTH)).max(px(0.));
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
                                    // Capture horizontal scroll wheel /
                                    // trackpad gestures and shift the
                                    // rows by adjusting h_offset.
                                    .on_scroll_wheel(cx.listener(
                                        move |this, ev: &gpui::ScrollWheelEvent, _w, cx| {
                                            let line_h = px(LISTING_ROW_HEIGHT);
                                            let dx = ev.delta.pixel_delta(line_h).x;
                                            if dx != px(0.) {
                                                this.scroll_h_by(-dx, max_h, cx);
                                            }
                                        },
                                    ))
                                    .child({
                                        let ctx = RowCtx {
                                            bundle: bundle.clone(),
                                            artifact: artifact.clone(),
                                            shell: cx.entity().downgrade(),
                                            selected_row,
                                        };
                                        list(scroll, move |index, _window, _cx| {
                                            let Some(row) = listing_rows.get(index)
                                            else {
                                                return div().into_any();
                                            };
                                            render_listing_row_with(
                                                row, index, h_offset, Some(&ctx),
                                            )
                                                .into_any()
                                        })
                                        .size_full()
                                    })
                                    .child(v_scrollbar),
                            )
                            .child(h_scrollbar)
                            .into_any_element()
                    }
                    (None, Some(progress)) => shell
                        .render_progress(&progress, panel, border, fg, dim, accent)
                        .into_any_element(),
                    (None, None) => div().flex_1().into_any_element(),
                }
            }
            Some(TabKind::SmaliClass { .. }) | None => {
                let active_class_jni: Option<String> = shell
                    .active_tab
                    .and_then(|i| shell.tabs.get(i))
                    .and_then(|t| match &t.kind {
                        TabKind::SmaliClass { class_jni } => Some(class_jni.clone()),
                        _ => None,
                    });
                let (right_state, right_lines, h_offset, selected_row) = match shell
                    .active_tab
                    .and_then(|i| shell.tabs.get(i))
                {
                    Some(tab) => (
                        tab.scroll.clone(),
                        tab.lines.clone().unwrap_or_else(|| Arc::new(Vec::new())),
                        tab.h_offset,
                        tab.selected_row,
                    ),
                    None => (
                        ListState::new(0, ListAlignment::Top, px(2000.)),
                        Arc::new(Vec::new()),
                        px(0.),
                        None,
                    ),
                };
                let shell_weak = cx.entity().downgrade();
                let v_scrollbar = list_scrollbar(&right_state, border, dim);
                let h_scrollbar = horizontal_scrollbar_offset(
                    h_offset,
                    px(LISTING_ROW_MIN_WIDTH),
                    border,
                    dim,
                );
                let max_h = px(LISTING_ROW_MIN_WIDTH);
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
                                    let dx = ev.delta.pixel_delta(px(22.)).x;
                                    if dx != px(0.) {
                                        this.scroll_h_by(-dx, max_h, cx);
                                    }
                                },
                            ))
                            .child(
                                list(right_state, {
                                    let lines = right_lines.clone();
                                    let shell_weak = shell_weak.clone();
                                    let bundle = bundle.clone();
                                    move |index, _window, _cx| {
                                        let text = lines
                                            .get(index)
                                            .cloned()
                                            .unwrap_or_else(|| SharedString::from(""));
                                        let is_selected =
                                            selected_row == Some(index);
                                        let mut row = div()
                                            .id(("smali-row", index))
                                            .h(px(22.))
                                            .w_full()
                                            .overflow_hidden()
                                            .relative();
                                        if is_selected {
                                            row = row.bg(rgb(COLOUR_ROW_SELECTED));
                                        }
                                        let weak = shell_weak.clone();
                                        // Tokenise the line and build a
                                        // flex-row of coloured chunks. Same
                                        // shape as the listing renderer.
                                        let tokens = tokenize_smali_line(text.as_ref());
                                        let mut inner = div()
                                            .absolute()
                                            .top_0()
                                            .left(-h_offset)
                                            .h(px(22.))
                                            .w(px(LISTING_ROW_MIN_WIDTH))
                                            .px_3()
                                            .text_base()
                                            .font_family("Courier New")
                                            .text_color(rgb(COLOUR_PLAIN))
                                            .whitespace_nowrap()
                                            .flex()
                                            .flex_row()
                                            .items_center();
                                        for (tok_idx, tok) in tokens.into_iter().enumerate() {
                                            // Class-ref Type chunks get
                                            // resolved against the bundle.
                                            // Internal classes are bright
                                            // and clickable; externals are
                                            // dimmed and inert.
                                            // MethodName chunk: render
                                            // clickable+underlined when the
                                            // `target_text` (`Class;->name(sig)ret`)
                                            // resolves to a known method line.
                                            if tok.kind == glass_arch_arm64::ChunkKind::MethodName
                                            {
                                                let key = tok.target_text.clone();
                                                let location: Option<(LeafId, usize)> = key
                                                    .as_ref()
                                                    .and_then(|k| bundle.method_lines.get(k))
                                                    .copied();
                                                let base_div = div()
                                                    .text_color(rgb(if location.is_some() {
                                                        COLOUR_PLAIN
                                                    } else {
                                                        COLOUR_PLAIN
                                                    }))
                                                    .whitespace_nowrap()
                                                    .child(SharedString::from(
                                                        tok.text.clone(),
                                                    ));
                                                if let Some((target_leaf, line_no)) = location {
                                                    let weak = weak.clone();
                                                    let tooltip_label = key
                                                        .as_ref()
                                                        .map(|s| format!("goto {s}"))
                                                        .unwrap_or_default();
                                                    let chip = base_div
                                                        .id((
                                                            "smali-method",
                                                            index * 1024 + tok_idx,
                                                        ))
                                                        .cursor_pointer()
                                                        .hover(|s| s.underline())
                                                        .on_mouse_down(
                                                            gpui::MouseButton::Left,
                                                            move |_ev, _w, cx: &mut App| {
                                                                cx.stop_propagation();
                                                                let Some(entity) =
                                                                    weak.upgrade()
                                                                else {
                                                                    return;
                                                                };
                                                                cx.update_entity(
                                                                    &entity,
                                                                    |shell, cx| {
                                                                        shell.goto_smali_method(
                                                                            target_leaf,
                                                                            line_no,
                                                                            cx,
                                                                        );
                                                                    },
                                                                );
                                                            },
                                                        )
                                                        .tooltip(move |_w, cx| {
                                                            cx.new(|_| TextTooltip {
                                                                text: SharedString::from(
                                                                    tooltip_label.clone(),
                                                                ),
                                                            })
                                                            .into()
                                                        });
                                                    inner = inner.child(chip);
                                                } else {
                                                    inner = inner.child(base_div);
                                                }
                                                continue;
                                            }
                                            if tok.kind == glass_arch_arm64::ChunkKind::Type {
                                                if let Some(jni) = extract_class_jni(&tok.text) {
                                                    let resolves = bundle
                                                        .resolve(
                                                            &glass_db::TabState::SmaliClass {
                                                                class_jni: jni.to_string(),
                                                            },
                                                        )
                                                        .is_some();
                                                    let colour = if resolves {
                                                        COLOUR_TYPE
                                                    } else {
                                                        COLOUR_TYPE_EXTERNAL
                                                    };
                                                    if resolves {
                                                        let jni = jni.to_string();
                                                        let dotted = jni_to_dotted(&jni);
                                                        let tooltip_label =
                                                            format!("goto {dotted}");
                                                        let weak = weak.clone();
                                                        let chip = div()
                                                            .id((
                                                                "smali-type",
                                                                index * 1024 + tok_idx,
                                                            ))
                                                            .text_color(rgb(colour))
                                                            .whitespace_nowrap()
                                                            .cursor_pointer()
                                                            .hover(|s| s.underline())
                                                            .child(SharedString::from(
                                                                tok.text.clone(),
                                                            ))
                                                            .on_mouse_down(
                                                                gpui::MouseButton::Left,
                                                                move |_ev, _w, cx: &mut App| {
                                                                    cx.stop_propagation();
                                                                    let Some(entity) =
                                                                        weak.upgrade()
                                                                    else {
                                                                        return;
                                                                    };
                                                                    let jni = jni.clone();
                                                                    cx.update_entity(
                                                                        &entity,
                                                                        |shell, cx| {
                                                                            if let Some(leaf) =
                                                                                shell.bundle().and_then(|b| {
                                                                                    b.resolve(
                                                                                        &glass_db::TabState::SmaliClass {
                                                                                            class_jni: jni.clone(),
                                                                                        },
                                                                                    )
                                                                                })
                                                                            {
                                                                                shell.open_leaf(
                                                                                    leaf, cx,
                                                                                );
                                                                            }
                                                                        },
                                                                    );
                                                                },
                                                            )
                                                            .tooltip(
                                                                move |_w, cx| {
                                                                    cx.new(|_| TextTooltip {
                                                                        text: SharedString::from(
                                                                            tooltip_label.clone(),
                                                                        ),
                                                                    })
                                                                    .into()
                                                                },
                                                            );
                                                        inner = inner.child(chip);
                                                        continue;
                                                    } else {
                                                        // External — render dimmed.
                                                        inner = inner.child(
                                                            div()
                                                                .text_color(rgb(colour))
                                                                .whitespace_nowrap()
                                                                .child(SharedString::from(
                                                                    tok.text,
                                                                )),
                                                        );
                                                        continue;
                                                    }
                                                }
                                            }
                                            inner = inner.child(
                                                div()
                                                    .text_color(rgb(chunk_colour(tok.kind)))
                                                    .whitespace_nowrap()
                                                    .child(SharedString::from(tok.text)),
                                            );
                                        }
                                        let right_weak = weak.clone();
                                        let right_lines = lines.clone();
                                        let right_class = active_class_jni.clone();
                                        row.on_mouse_down(
                                            gpui::MouseButton::Left,
                                            move |_ev, _w, cx: &mut App| {
                                                if let Some(entity) = weak.upgrade() {
                                                    cx.update_entity(
                                                        &entity,
                                                        |shell, cx| {
                                                            shell.select_active_row(
                                                                index, cx,
                                                            );
                                                        },
                                                    );
                                                }
                                            },
                                        )
                                        .on_mouse_down(
                                            gpui::MouseButton::Right,
                                            move |ev: &gpui::MouseDownEvent,
                                                  _w,
                                                  cx: &mut App| {
                                                let Some(class_jni) =
                                                    right_class.clone()
                                                else {
                                                    return;
                                                };
                                                // Walk upward from the
                                                // right-clicked line to find
                                                // the most recent `.method`
                                                // declaration. That's the
                                                // method containing this
                                                // line.
                                                let mut method_decl: Option<String> = None;
                                                for j in (0..=index).rev() {
                                                    let Some(line) = right_lines.get(j) else { continue };
                                                    let trimmed = line.trim_start();
                                                    if let Some(after) =
                                                        trimmed.strip_prefix(".method ")
                                                    {
                                                        if let Some(decl) = after
                                                            .split_whitespace()
                                                            .last()
                                                        {
                                                            method_decl = Some(decl.to_string());
                                                        }
                                                        break;
                                                    }
                                                    if trimmed.starts_with(".end method") {
                                                        // We're outside any
                                                        // method; no menu.
                                                        return;
                                                    }
                                                }
                                                let Some(method_decl) = method_decl else { return };
                                                let pos = ev.position;
                                                if let Some(entity) = right_weak.upgrade() {
                                                    cx.update_entity(
                                                        &entity,
                                                        |shell, cx| {
                                                            shell.open_smali_context_menu(
                                                                class_jni,
                                                                method_decl,
                                                                pos,
                                                                cx,
                                                            );
                                                        },
                                                    );
                                                }
                                            },
                                        )
                                        .child(inner)
                                        .into_any()
                                    }
                                })
                                .size_full(),
                            )
                            .child(v_scrollbar),
                    )
                    .child(h_scrollbar)
                    .into_any_element()
            }
            Some(TabKind::Cfg { artifact, entry_addr }) => shell
                .render_cfg(&bundle, &artifact, entry_addr, panel, border, fg, dim, cx)
                .into_any_element(),
            Some(TabKind::DexCallGraph {
                class_jni,
                method_decl,
            }) => shell
                .render_dex_callgraph(
                    &bundle,
                    &class_jni,
                    &method_decl,
                    panel,
                    border,
                    fg,
                    dim,
                    accent,
                    cx,
                )
                .into_any_element(),
        };

        let right = div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .relative()
            .child(tab_bar)
            .child(body)
            .child(overflow_dropdown);

        div()
            .flex_1()
            .flex()
            .flex_row()
            .overflow_hidden()
            .child(left)
            .child(right)
            .into_any_element()
}
