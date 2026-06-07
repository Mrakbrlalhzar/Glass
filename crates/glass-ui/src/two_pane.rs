//! Two-pane layout: tree on the left, body content on the right.
//!
//! Extracted from `Shell::render_two_pane` as a free function taking
//! `&mut Shell`. The body dispatches on the active tab kind to the
//! per-view renderers (listing, hex, smali, manifest, section_map,
//! cfg, dex_callgraph).

use std::sync::Arc;

use gpui::{
    div, list, prelude::*, px, App, Context, ListAlignment, ListState, SharedString,
};

use crate::listing_render::{
    render_hex_row, render_listing_row_with, RowCtx, HEX_ROW_HEIGHT, HEX_ROW_MIN_WIDTH,
    LISTING_ROW_HEIGHT, LISTING_ROW_MIN_WIDTH,
};
use crate::scrollbar::{horizontal_scrollbar_offset, list_scrollbar};
use crate::{
    flatten, LoadedBundle, RowAction, RowKind, Shell, TabKind, VisibleRow,
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
        // Scripts section lives below the bundle tree, in the same
        // flex column. Rendered above the list element so it sits
        // *under* the tree visually (the tree gets `flex_1`, the
        // scripts panel gets its natural height).
        let scripts_panel_el =
            crate::scripts_panel::render_panel(shell, panel, border, fg, dim, accent, cx);

        // Top-of-navigator "Coverage Map" entry. Always
        // visible (whether or not the tab is currently open);
        // clicking it opens or focuses the singleton tab.
        // Lit when the coverage map is the active tab.
        let coverage_active = shell
            .active_tab
            .and_then(|i| shell.tabs.get(i))
            .map(|t| matches!(t.kind, crate::TabKind::CoverageMap))
            .unwrap_or(false);
        let coverage_bg = if coverage_active { accent } else { panel };
        let coverage_fg = if coverage_active {
            crate::theme::current().shell.text_bright.rgba()
        } else {
            fg
        };
        let coverage_handle = cx.entity().downgrade();
        let coverage_row = div()
            .h(px(22.))
            .w_full()
            .pl(px(8.))
            .pr_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .text_xs()
            .bg(coverage_bg)
            .text_color(coverage_fg)
            .border_b_1()
            .border_color(border)
            .child(
                div().w(px(14.)).h(px(14.)).flex_shrink_0().child(
                    gpui::svg()
                        .path(SharedString::from("icons/section-map.svg"))
                        .size_full()
                        .text_color(coverage_fg),
                ),
            )
            .child(SharedString::from("Coverage Map"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |_ev, _w, cx: &mut App| {
                    let Some(entity) = coverage_handle.upgrade() else { return };
                    cx.update_entity(&entity, |shell, cx| {
                        shell.open_coverage_map(cx);
                    });
                },
            );

        let left = div()
            .w(shell.left_pane_width)
            .h_full()
            .flex_shrink_0()
            .relative()
            .border_r_1()
            .border_color(border)
            .bg(panel)
            .child(
                div().size_full().flex().flex_col()
                .child(coverage_row)
                .child(
                list(shell.list_state.clone(), {
                    let rows = rows.clone();
                    let leaf_icons = bundle.leaf_icons.clone();
                    move |index, _window, _cx| {
                        let row = rows[index].clone();
                        let handle = self_handle.clone();
                        let indent = px(8. + row.depth as f32 * 14.);

                        let (is_selected, label, on_click_kind, leaf_icon): (bool, SharedString, RowAction, Option<&'static str>) = match row.kind {
                            RowKind::Group { ref path, expanded: _, ref label } => (
                                false,
                                label.clone(),
                                RowAction::Toggle(path.clone()),
                                None,
                            ),
                            RowKind::Leaf { leaf_id, ref label } => (
                                selected == Some(leaf_id),
                                label.clone(),
                                RowAction::Select(leaf_id),
                                leaf_icons.get(leaf_id.0).copied(),
                            ),
                        };
                        let chevron = match row.kind {
                            RowKind::Group { expanded, .. } => {
                                if expanded { Some("▾") } else { Some("▸") }
                            }
                            RowKind::Leaf { .. } => None,
                        };

                        let row_bg = if is_selected { accent } else { panel };
                        let row_fg = if is_selected { crate::theme::current().shell.text_bright.rgba() } else { fg };

                        let mut row_div = div()
                            .h(px(22.))
                            .w_full()
                            .pl(indent)
                            .pr_3()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .bg(row_bg)
                            .text_color(row_fg);
                        if let Some(c) = chevron {
                            row_div = row_div.child(
                                div().w(px(14.)).flex_shrink_0().child(c),
                            );
                        } else if let Some(icon_path) = leaf_icon {
                            row_div = row_div.child(
                                div()
                                    .w(px(14.))
                                    .h(px(14.))
                                    .flex_shrink_0()
                                    .child(
                                        gpui::svg()
                                            .path(SharedString::from(icon_path))
                                            .size_full()
                                            .text_color(row_fg),
                                    ),
                            );
                        } else {
                            row_div = row_div.child(
                                div().w(px(14.)).flex_shrink_0(),
                            );
                        }
                        row_div
                            .child(label)
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
                )
                .child(
                    // Top-border separator so the scripts section
                    // reads as visually distinct from the tree.
                    div()
                        .w_full()
                        .border_t_1()
                        .border_color(border)
                        .child(scripts_panel_el),
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
            Some(TabKind::ObjCClass { .. }) => {
                crate::objc_view::render_objc_tab(shell, bundle.clone(), cx, border, dim, accent)
            }
            Some(TabKind::SwiftType { .. }) => {
                crate::swift_view::render_swift_tab(shell, bundle.clone(), cx, border, dim, accent)
            }
            Some(TabKind::CoverageMap) => {
                crate::coverage_view::render_coverage_tab(
                    shell, bundle.clone(), panel, border, fg, dim, cx,
                )
                .into_any_element()
            }
            Some(TabKind::SectionMap { artifact }) => shell
                .render_section_map(&bundle, &artifact, panel, border, fg, dim, cx)
                .into_any_element(),
            Some(TabKind::Hex { artifact, .. }) => {
                let (paged_opt, scroll_opt, h_offset, selected_row, selected_byte) =
                    match shell.active_tab.and_then(|i| shell.tabs.get(i)) {
                        Some(tab) => (
                            tab.hex_paged.clone(),
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
                let ctx = RowCtx {
                    bundle: bundle.clone(),
                    artifact: artifact.clone(),
                    shell: cx.entity().downgrade(),
                    selected_row,
                    disasm_edit: shell.disasm_edit.clone(),
                    hex_edit: shell.hex_edit.clone(),
                };
                // `paged` is None on the first frame after tab
                // open (before `ensure_active_tab_lines` runs);
                // the renderer treats that as an empty list.
                let paged = paged_opt.unwrap_or_else(|| {
                    // A short-lived placeholder paged with no
                    // rows. Cheaper than reshaping the callback
                    // to take Option.
                    Arc::new(crate::paged_hex::PagedHex::new(
                        crate::DataSectionBytes {
                            base: 0,
                            bytes: Arc::new(Vec::new()),
                            kind: crate::NativeSectionKind::Data,
                        },
                        Arc::new(glass_arch_arm::SymbolMap::default()),
                        1,
                    ))
                });
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
                                    // page_for_row_blocking materialises
                                    // the containing page if it isn't
                                    // cached. Worst case: first scroll
                                    // into a new page blocks for a few
                                    // ms while build_hex_rows runs over
                                    // 16 KB of bytes. Step 3 will move
                                    // this to a non-blocking lookup +
                                    // background build.
                                    let Some((page, off)) =
                                        paged.page_for_row_blocking(index as u32)
                                    else {
                                        return div().into_any();
                                    };
                                    render_hex_row(
                                        &page[off],
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
                    disasm_edit: shell.disasm_edit.clone(),
                    hex_edit: shell.hex_edit.clone(),
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
            Some(TabKind::ScriptEditor { .. })
            | Some(TabKind::SmaliEditor { .. })
            | Some(TabKind::PlistEditor { .. })
            | Some(TabKind::ManifestEditor { .. }) => {
                let editor = shell
                    .active_tab
                    .and_then(|i| shell.tabs.get(i))
                    .and_then(|t| t.code_editor.as_ref());
                match editor {
                    Some(e) => crate::code_editor::render_code_editor(
                        e, panel, border, fg, dim, cx,
                    ),
                    // Tab was opened without an editor seeded —
                    // shouldn't happen via `open_*_editor`, but
                    // render an empty placeholder rather than panic.
                    None => div().flex_1().into_any_element(),
                }
            }
            // No active tab: render an empty placeholder.
            None => div().flex_1().into_any_element(),
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
            // min_w(0) so this column shrinks below the tab bar's
            // intrinsic content width when the user opens a project
            // with many restored tabs — without it, the column
            // would expand to fit every tab side-by-side and the
            // overflow logic never triggers.
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .relative()
            .overflow_hidden()
            .child(tab_bar)
            .child(body)
            .child(overflow_dropdown);

        let pane_open = shell.annotations_pane_open;
        let pane = if pane_open {
            Some(crate::annotations_pane::render_annotations_pane(
                &*shell, &bundle, cx, panel, border, fg, dim,
            ))
        } else {
            None
        };

        // Splitter handle between the left nav and right body.
        // 5px-wide hit target with a ResizeLeftRight cursor; the
        // left pane's right border carries the visible line.
        // mouse-down anchors the drag; move + up are listened on
        // the outer container below so the pointer can leave
        // this 5px zone mid-drag without breaking the gesture.
        let splitter = div()
            .id("left-pane-splitter")
            .w(px(5.))
            .h_full()
            .flex_shrink_0()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|shell, ev: &gpui::MouseDownEvent, _w, cx| {
                    shell.start_left_pane_resize(ev.position.x, cx);
                }),
            );
        let dragging = shell.left_pane_resize_anchor.is_some();
        let mut outer = div()
            .flex_1()
            .flex()
            .flex_row()
            .overflow_hidden()
            .child(left)
            .child(splitter)
            .child(right)
            // Track mouse moves window-wide while the splitter
            // drag is in progress. Pointer can travel anywhere
            // (faster than 5px / event) without losing the
            // gesture; move + up stay routed to Shell.
            .on_mouse_move(cx.listener(
                |shell, ev: &gpui::MouseMoveEvent, _w, cx| {
                    if shell.left_pane_resize_anchor.is_some()
                        && ev.pressed_button == Some(gpui::MouseButton::Left)
                    {
                        shell.update_left_pane_resize(ev.position.x, cx);
                    }
                },
            ))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|shell, _ev, _w, cx| {
                    if shell.left_pane_resize_anchor.is_some() {
                        shell.finish_left_pane_resize(cx);
                    }
                }),
            );
        // While dragging, force the resize cursor over the whole
        // body so it doesn't flicker back to the default when the
        // pointer crosses over the right pane.
        if dragging {
            outer = outer.cursor(gpui::CursorStyle::ResizeLeftRight);
        }
        if let Some(p) = pane {
            outer = outer.child(p);
        }
        outer.into_any_element()
}

