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
                                        .text_color(rgb(COLOUR_PLAIN()))
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
                    disasm_edit: shell.disasm_edit.clone(),
                    hex_edit: shell.hex_edit.clone(),
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
            Some(TabKind::SmaliClass { .. }) | None => {
                let active_class_jni: Option<String> = shell
                    .active_tab
                    .and_then(|i| shell.tabs.get(i))
                    .and_then(|t| match &t.kind {
                        TabKind::SmaliClass { class_jni } => Some(class_jni.clone()),
                        _ => None,
                    });
                // Snapshot the active op-edit state so the per-row
                // closure can swap in the inline editor when the
                // row matches.
                let op_edit_snapshot: Option<crate::op_editor::OpEditState> =
                    shell.op_edit.clone();
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
                // Pre-compute a per-row annotation snapshot for
                // this smali leaf. The lookup walks lines once,
                // tracks the current .method / .class state, and
                // stores `Some(annotation)` for any row that
                // matches a Class / Method / MethodLine key. All
                // other rows hold `None`.
                let row_annotations: Arc<Vec<Option<glass_db::Annotation>>> = Arc::new(
                    build_smali_row_annotations(
                        &bundle,
                        active_class_jni.as_deref(),
                        &right_lines,
                    ),
                );
                // Does the active class have a staged smali edit?
                // If so we tint its class-declaration lines so the
                // user can see which class headers are modified at
                // a glance. Same pattern as the disasm editor's
                // green wash on edited rows.
                // Tint class-decl rows only when the staged edit
                // actually differs in the class-declaration portion
                // (modifiers / super / implements / source / class
                // annotations). A pure field or method edit shouldn't
                // light up the `.class` / `.super` / … rows.
                // Per-row structural-scope mask. Tells us, for
                // each line in the leaf, whether the row belongs
                // to the class declaration, a specific field, a
                // specific method, or none of the above. Computed
                // once per render; the per-row closures index in
                // by row index.
                let row_scopes: Arc<Vec<crate::smali_row_scope::RowScope>> =
                    Arc::new(crate::smali_row_scope::compute(&right_lines));
                // Find which class members are actually edited
                // (class-decl portion, individual fields, individual
                // methods). Tinting compares each row's scope
                // against these sets.
                let class_decl_edited: bool;
                let edited_fields: std::collections::HashSet<(String, String)>;
                let edited_methods: std::collections::HashSet<(String, String)>;
                if let Some(jni) = active_class_jni.as_deref() {
                    let (cd, fs, ms) = bundle
                        .smali_classes
                        .iter()
                        .find_map(|((aid, j), original)| {
                            if j != jni {
                                return None;
                            }
                            Some((
                                bundle.smali_edits.class_decl_differs(aid, j, original),
                                bundle.smali_edits.edited_fields(aid, j, original),
                                bundle.smali_edits.edited_methods(aid, j, original),
                            ))
                        })
                        .unwrap_or((false, Vec::new(), Vec::new()));
                    class_decl_edited = cd;
                    edited_fields = fs.into_iter().collect();
                    edited_methods = ms.into_iter().collect();
                } else {
                    class_decl_edited = false;
                    edited_fields = Default::default();
                    edited_methods = Default::default();
                }
                let edited_fields = Arc::new(edited_fields);
                let edited_methods = Arc::new(edited_methods);
                // Live-trace mask — set of (method_name, signature)
                // pairs on the current class that have an
                // active/pending Frida trace. Drives the magenta
                // row tint so the user can see at a glance which
                // methods are instrumented.
                let traced_methods: Arc<
                    std::collections::HashSet<(String, String)>,
                > = if let Some(jni) = active_class_jni.as_deref() {
                    let set = bundle
                        .traces
                        .entries()
                        .iter()
                        .filter(|e| {
                            e.key.class_jni == jni
                                && matches!(
                                    e.status,
                                    crate::traces::TraceStatus::Pending
                                        | crate::traces::TraceStatus::Active
                                )
                        })
                        .map(|e| {
                            (
                                e.key.method_name.clone(),
                                e.key.method_signature.clone(),
                            )
                        })
                        .collect();
                    Arc::new(set)
                } else {
                    Arc::new(std::collections::HashSet::new())
                };
                // Hook mask — same shape as traces. Hooks
                // win over traces because changing behaviour
                // is the higher-attention state.
                let hooked_methods: Arc<
                    std::collections::HashSet<(String, String)>,
                > = if let Some(jni) = active_class_jni.as_deref() {
                    let set = bundle
                        .hooks
                        .entries()
                        .iter()
                        .filter(|e| {
                            e.key.class_jni == jni
                                && matches!(
                                    e.status,
                                    crate::hooks::HookStatus::Pending
                                        | crate::hooks::HookStatus::Active
                                )
                        })
                        .map(|e| {
                            (
                                e.key.method_name.clone(),
                                e.key.method_signature.clone(),
                            )
                        })
                        .collect();
                    Arc::new(set)
                } else {
                    Arc::new(std::collections::HashSet::new())
                };
                // Origin chip — shows which DEX inside the APK
                // this class was lifted from. Tooltip-style label
                // anchored top-right of the smali body. We surface
                // it because the user no longer sees per-DEX
                // groupings in the navigator (all classes share a
                // single package tree now), but they may still
                // want to know "is this in classes.dex or
                // classes3.dex?" when sizing a patch.
                let active_leaf_origin: Option<SharedString> = shell
                    .active_leaf()
                    .and_then(|leaf| bundle.origins.get(leaf.0).cloned());
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
                            .when_some(active_leaf_origin, |d, origin| {
                                d.child(
                                    div()
                                        .absolute()
                                        .top(px(4.))
                                        .right(px(12.))
                                        .px_2()
                                        .py_0p5()
                                        .text_xs()
                                        .text_color(dim)
                                        .border_1()
                                        .border_color(border)
                                        .rounded_sm()
                                        .bg(panel)
                                        .child(origin),
                                )
                            })
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
                                    let active_class_jni = active_class_jni.clone();
                                    let row_annotations = row_annotations.clone();
                                    let row_scopes = row_scopes.clone();
                                    let edited_fields = edited_fields.clone();
                                    let edited_methods = edited_methods.clone();
                                    let traced_methods = traced_methods.clone();
                                    let hooked_methods = hooked_methods.clone();
                                    let op_edit_snapshot = op_edit_snapshot.clone();
                                    move |index, _window, _cx| {
                                        // Inline op editor — when this row is
                                        // the active op-edit target, render a
                                        // TextInput row instead of the normal
                                        // syntax-highlighted line. Keystrokes
                                        // reach the editor via the Shell-level
                                        // on_key_down listener.
                                        if let Some(op_state) = op_edit_snapshot.as_ref() {
                                            if op_state.row_index == index {
                                                let bg = crate::theme::current()
                                                    .state
                                                    .committed_bg
                                                    .rgba();
                                                let bg = gpui::Rgba {
                                                    r: bg.r,
                                                    g: bg.g,
                                                    b: bg.b,
                                                    a: 0.7,
                                                };
                                                let fg = crate::theme::current()
                                                    .shell
                                                    .text_bright
                                                    .rgba();
                                                let dim = crate::theme::current()
                                                    .shell
                                                    .text_dim
                                                    .rgba();
                                                return crate::op_editor::render_row(
                                                    op_state, bg, fg, dim,
                                                );
                                            }
                                        }
                                        let text = lines
                                            .get(index)
                                            .cloned()
                                            .unwrap_or_else(|| SharedString::from(""));
                                        let is_selected =
                                            selected_row == Some(index);
                                        // Per-row annotation pre-computed for
                                        // this leaf (see build_smali_row_
                                        // annotations below). Same edge-dot
                                        // / tint / comment treatment as the
                                        // listing — but keyed on the smali
                                        // line offset, not address.
                                        let annotation = row_annotations
                                            .get(index)
                                            .cloned()
                                            .flatten();
                                        let mut row = div()
                                            .id(("smali-row", index))
                                            .h(px(22.))
                                            .w_full()
                                            .overflow_hidden()
                                            .relative();
                                        // Tint priority:
                                        //   selected > hooked (crimson)
                                        //   > traced (magenta) > edited
                                        //   > annotation > none.
                                        // Hooks beat traces because
                                        // modifying behaviour is more
                                        // dangerous than observing it.
                                        let scope_method = match row_scopes.get(index) {
                                            Some(crate::smali_row_scope::RowScope::Method { name, signature }) => {
                                                Some((name.clone(), signature.clone()))
                                            }
                                            _ => None,
                                        };
                                        let is_hooked_row = scope_method
                                            .as_ref()
                                            .map(|m| hooked_methods.contains(m))
                                            .unwrap_or(false);
                                        let is_traced_row = !is_hooked_row
                                            && scope_method
                                                .as_ref()
                                                .map(|m| traced_methods.contains(m))
                                                .unwrap_or(false);
                                        if is_selected {
                                            row = row.bg(rgb(COLOUR_ROW_SELECTED()));
                                        } else if is_hooked_row {
                                            // Crimson — louder than
                                            // magenta because hooks
                                            // change behaviour.
                                            row = row.bg(gpui::Rgba {
                                                r: 0.85,
                                                g: 0.15,
                                                b: 0.20,
                                                a: 0.22,
                                            });
                                        } else if is_traced_row {
                                            // Magenta — distinct from
                                            // edit-green (instrumentation
                                            // is a live observation, not
                                            // a pending change).
                                            row = row.bg(gpui::Rgba {
                                                r: 0.85,
                                                g: 0.30,
                                                b: 0.85,
                                                a: 0.18,
                                            });
                                        } else if match row_scopes.get(index) {
                                            Some(crate::smali_row_scope::RowScope::ClassDecl) => class_decl_edited,
                                            Some(crate::smali_row_scope::RowScope::Field { name, signature }) => {
                                                edited_fields.contains(&(name.clone(), signature.clone()))
                                            }
                                            Some(crate::smali_row_scope::RowScope::Method { name, signature }) => {
                                                edited_methods.contains(&(name.clone(), signature.clone()))
                                            }
                                            _ => false,
                                        } {
                                            // Green wash — same idiom as the
                                            // disasm editor uses for staged
                                            // instruction edits, at ~50% alpha
                                            // so the syntax tokens stay legible.
                                            let bg = crate::theme::current().state.committed_bg.rgba();
                                            row = row.bg(gpui::Rgba { r: bg.r, g: bg.g, b: bg.b, a: 0.5 });
                                        } else if let Some(rgba) =
                                            annotation.as_ref().and_then(|a| a.colour)
                                        {
                                            // Dim alpha to ~24% like the listing.
                                            let dimmed = (rgba & 0xffffff00) | 0x3c;
                                            row = row.bg(gpui::rgba(dimmed));
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
                                            .text_color(rgb(COLOUR_PLAIN()))
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
                                            if tok.kind == glass_arch_arm::ChunkKind::MethodName
                                            {
                                                let key = tok.target_text.clone();
                                                let location: Option<(LeafId, usize)> = key
                                                    .as_ref()
                                                    .and_then(|k| bundle.method_lines.get(k))
                                                    .copied();
                                                let base_div = div()
                                                    .text_color(rgb(if location.is_some() {
                                                        COLOUR_PLAIN()
                                                    } else {
                                                        COLOUR_PLAIN()
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
                                                            move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                                                                // Let double-clicks bubble
                                                                // to the row so the per-op
                                                                // editor opens.
                                                                if ev.click_count >= 2 {
                                                                    return;
                                                                }
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
                                            if tok.kind == glass_arch_arm::ChunkKind::Type {
                                                if let Some(jni) = extract_class_jni(&tok.text) {
                                                    let resolves = bundle
                                                        .resolve(
                                                            &glass_db::TabState::SmaliClass {
                                                                class_jni: jni.to_string(),
                                                                scroll_line: 0,
                                                            },
                                                        )
                                                        .is_some();
                                                    let colour = if resolves {
                                                        COLOUR_TYPE()
                                                    } else {
                                                        COLOUR_TYPE_EXTERNAL()
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
                                                                move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                                                                    // Let double-clicks bubble
                                                                    // to the row so the per-op
                                                                    // editor opens.
                                                                    if ev.click_count >= 2 {
                                                                        return;
                                                                    }
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
                                                                                            scroll_line: 0,
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
                                        // User comment hung off the smali
                                        // class / method key is appended
                                        // after the tokenised line so the
                                        // syntax-coloured content stays
                                        // unmolested.
                                        if let Some(comment) =
                                            annotation.as_ref().and_then(|a| a.comment.as_deref())
                                        {
                                            inner = inner.child(
                                                div()
                                                    .ml_4()
                                                    .text_color(rgb(crate::palette::COLOUR_COMMENT()))
                                                    .whitespace_nowrap()
                                                    .child(SharedString::from(
                                                        format!("; {comment}"),
                                                    )),
                                            );
                                        }
                                        let right_weak = weak.clone();
                                        let right_lines = lines.clone();
                                        let right_class = active_class_jni.clone();
                                        let right_scopes = row_scopes.clone();
                                        let dbl_lines = lines.clone();
                                        let dbl_weak = weak.clone();
                                        let dbl_scopes = row_scopes.clone();
                                        let row = row.on_mouse_down(
                                            gpui::MouseButton::Left,
                                            move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                                                // Single-click selects the row.
                                                if let Some(entity) = dbl_weak.upgrade() {
                                                    cx.update_entity(
                                                        &entity,
                                                        |shell, cx| {
                                                            shell.select_active_row(
                                                                index, cx,
                                                            );
                                                        },
                                                    );
                                                }
                                                // Double-click opens a structural
                                                // editor when the row is part of
                                                // the class declaration (`.class`,
                                                // `.super`, `.implements`,
                                                // `.source`). Field / method
                                                // headers are M1.4 / M1.5 — for
                                                // now they're inert on double-click.
                                                if ev.click_count < 2 {
                                                    return;
                                                }
                                                let Some(text) = dbl_lines.get(index) else {
                                                    return;
                                                };
                                                if matches!(
                                                    dbl_scopes.get(index),
                                                    Some(crate::smali_row_scope::RowScope::ClassDecl)
                                                ) {
                                                    if let Some(entity) = dbl_weak.upgrade() {
                                                        cx.update_entity(
                                                            &entity,
                                                            |shell, cx| {
                                                                shell.open_class_decl_edit(cx);
                                                            },
                                                        );
                                                    }
                                                    return;
                                                }
                                                if crate::field_popover::line_is_field_decl(text) {
                                                    let line = text.clone();
                                                    if let Some(entity) = dbl_weak.upgrade() {
                                                        cx.update_entity(
                                                            &entity,
                                                            |shell, cx| {
                                                                shell.open_field_edit_for_line(line.as_ref(), cx);
                                                            },
                                                        );
                                                    }
                                                    return;
                                                }
                                                if crate::method_popover::line_is_method_decl(text) {
                                                    let line = text.clone();
                                                    if let Some(entity) = dbl_weak.upgrade() {
                                                        cx.update_entity(
                                                            &entity,
                                                            |shell, cx| {
                                                                shell.open_method_edit_for_line(line.as_ref(), cx);
                                                            },
                                                        );
                                                    }
                                                    return;
                                                }
                                                // Method body row — open the
                                                // per-op inline editor.
                                                if let Some(entity) = dbl_weak.upgrade() {
                                                    cx.update_entity(
                                                        &entity,
                                                        |shell, cx| {
                                                            shell.open_op_edit_for_row(index, cx);
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
                                                let pos = ev.position;
                                                // First: is this row part of
                                                // the class declaration
                                                // (`.class`, `.super`,
                                                // `.implements`, `.source`)?
                                                // All four share the same
                                                // annotation surface — keyed
                                                // on the class as a whole —
                                                // and the same Revert
                                                // affordance when the class
                                                // has a staged edit.
                                                if matches!(
                                                    right_scopes.get(index),
                                                    Some(crate::smali_row_scope::RowScope::ClassDecl)
                                                ) {
                                                    if let Some(entity) =
                                                        right_weak.upgrade()
                                                    {
                                                        cx.update_entity(
                                                            &entity,
                                                            |shell, cx| {
                                                                shell.open_smali_class_context_menu(
                                                                    class_jni.clone(),
                                                                    pos,
                                                                    cx,
                                                                );
                                                            },
                                                        );
                                                    }
                                                    return;
                                                }
                                                // First: is this row itself a
                                                // `.field` line? If so, show
                                                // "References to field".
                                                if let Some(row) = right_lines.get(index) {
                                                    let trimmed = row.trim_start();
                                                    if let Some(after) =
                                                        trimmed.strip_prefix(".field ")
                                                    {
                                                        if let Some(decl) = after
                                                            .split_whitespace()
                                                            .last()
                                                        {
                                                            let field_ref = format!(
                                                                "{class_jni}->{decl}"
                                                            );
                                                            let label =
                                                                decl.to_string();
                                                            if let Some(entity) =
                                                                right_weak.upgrade()
                                                            {
                                                                cx.update_entity(
                                                                    &entity,
                                                                    |shell, cx| {
                                                                        shell.open_field_context_menu(
                                                                            field_ref,
                                                                            label,
                                                                            pos,
                                                                            cx,
                                                                        );
                                                                    },
                                                                );
                                                            }
                                                            return;
                                                        }
                                                    }
                                                }
                                                // Right-click on a method
                                                // header line itself — show
                                                // the method-specific menu
                                                // (call-graph, callers,
                                                // Revert when staged).
                                                if let Some(row_text) =
                                                    right_lines.get(index)
                                                {
                                                    if row_text.trim_start().starts_with(".method ") {
                                                        if let Some(crate::smali_row_scope::RowScope::Method {
                                                            name, signature,
                                                        }) = right_scopes.get(index)
                                                        {
                                                            let name = name.clone();
                                                            let sig = signature.clone();
                                                            let display =
                                                                format!("{name}{sig}");
                                                            if let Some(entity) =
                                                                right_weak.upgrade()
                                                            {
                                                                cx.update_entity(
                                                                    &entity,
                                                                    |shell, cx| {
                                                                        shell.open_method_header_context_menu(
                                                                            name, sig, display, pos, cx,
                                                                        );
                                                                    },
                                                                );
                                                            }
                                                            return;
                                                        }
                                                    }
                                                }
                                                // Otherwise: find the enclosing
                                                // .method and pass its line
                                                // index so the annotation key
                                                // includes the row-relative
                                                // line offset.
                                                let mut method_decl: Option<String> = None;
                                                let mut method_line_idx: usize = index;
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
                                                            method_line_idx = j;
                                                        }
                                                        break;
                                                    }
                                                    if trimmed.starts_with(".end method") {
                                                        return;
                                                    }
                                                }
                                                let Some(method_decl) = method_decl else { return };
                                                let line_offset =
                                                    (index - method_line_idx) as u32;
                                                if let Some(entity) = right_weak.upgrade() {
                                                    cx.update_entity(
                                                        &entity,
                                                        |shell, cx| {
                                                            shell.open_smali_context_menu(
                                                                class_jni,
                                                                method_decl,
                                                                line_offset,
                                                                pos,
                                                                cx,
                                                            );
                                                        },
                                                    );
                                                }
                                            },
                                        );
                                        // Build the optional edge dot up
                                        // front so we can finalize the row
                                        // in one expression. The dot lives
                                        // on `row` (the clipped outer) so
                                        // it stays visible regardless of
                                        // horizontal scroll.
                                        let dot_child: Option<gpui::Div> =
                                            annotation.as_ref().map(|ann| {
                                                let dot_rgba =
                                                    ann.colour.unwrap_or(0x4f7cffff);
                                                div()
                                                    .absolute()
                                                    .top(px(7.))
                                                    .right(px(8.))
                                                    .w(px(8.))
                                                    .h(px(8.))
                                                    .rounded_full()
                                                    .bg(gpui::rgba(dot_rgba))
                                            });
                                        let row = row.child(inner);
                                        let row = if let Some(d) = dot_child {
                                            row.child(d)
                                        } else {
                                            row
                                        };
                                        row.into_any()
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

        let mut outer = div()
            .flex_1()
            .flex()
            .flex_row()
            .overflow_hidden()
            .child(left)
            .child(right);
        if let Some(p) = pane {
            outer = outer.child(p);
        }
        outer.into_any_element()
}

/// Walk the smali leaf's lines once and produce a parallel vector
/// where each index either holds the annotation that applies to
/// that line or `None`. The lookup keeps the current `.method`
/// state (key + header line index) so any row inside a method
/// body resolves to a `MethodLine(class, name+sig, offset)`
/// annotation. The `.class` header resolves to a `Class` key.
///
/// All other lines (directives, labels, blank lines) return
/// `None`, leaving the row untouched.
///
/// `MethodLine(_, _, 0)` and the bare `Method(_, _)` key are
/// treated as aliases: a v2 record set via MCP on `Method` still
/// renders on the `.method` header. The newer GUI writes always
/// use `MethodLine`.
fn build_smali_row_annotations(
    bundle: &LoadedBundle,
    active_class_jni: Option<&str>,
    lines: &[SharedString],
) -> Vec<Option<glass_db::Annotation>> {
    let mut out: Vec<Option<glass_db::Annotation>> = vec![None; lines.len()];
    let Some(aid) = bundle.artifact_ids.first() else { return out };
    let Some(idx) = bundle.annotations.get(aid) else { return out };
    let mut current_method_key: Option<String> = None;
    let mut current_method_line: usize = 0;
    // op_cursor counts the ops we've passed within the current
    // method body. Same classifier as `line_offset_to_op_index`
    // — every line that's not method-prelude / .end method
    // counts as one op (with `.array-data`/`.packed-switch`/
    // `.sparse-switch` blocks counted once and then skipped
    // until their closer).
    let mut op_cursor: u32 = 0;
    let mut skip_until: Option<&'static str> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(".class ") {
            if let Some(class_jni) = active_class_jni {
                out[i] = idx.at_class(class_jni).cloned();
            }
            continue;
        }
        if let Some(after) = trimmed.strip_prefix(".method ") {
            if let Some(class_jni) = active_class_jni {
                if let Some(decl) = after.split_whitespace().last() {
                    let key = format!("{class_jni}->{decl}");
                    current_method_key = Some(key.clone());
                    current_method_line = i;
                    op_cursor = 0;
                    skip_until = None;
                    // Header row: legacy `MethodLine(_, 0)` or
                    // `Method` both apply.
                    out[i] = idx
                        .at_method_line(&key, 0)
                        .cloned()
                        .or_else(|| idx.at_method(&key).cloned());
                    continue;
                }
            }
            continue;
        }
        if trimmed.starts_with(".end method") {
            current_method_key = None;
            op_cursor = 0;
            skip_until = None;
            continue;
        }
        let Some(key) = current_method_key.as_ref() else { continue };
        // Mid-body row classification — mirrors
        // `line_offset_to_op_index`.
        if let Some(closer) = skip_until {
            // Body of a multi-line op block. Annotation, if any,
            // belongs to the op whose opener we already counted
            // (op_cursor - 1).
            if let Some(prev_op) = op_cursor.checked_sub(1) {
                if let Some(a) = idx.at_op_index(key, prev_op) {
                    out[i] = Some(a.clone());
                }
            }
            if trimmed.starts_with(closer) {
                skip_until = None;
            }
            continue;
        }
        let prelude = trimmed.starts_with(".locals ")
            || trimmed.starts_with(".registers ")
            || trimmed.starts_with(".param")
            || trimmed.starts_with(".annotation ")
            || trimmed.starts_with(".end annotation")
            || trimmed.starts_with(".subannotation ")
            || trimmed.starts_with(".end subannotation")
            || trimmed.is_empty();
        if prelude {
            // Legacy MethodLine annotations may have been
            // attached to one of these rows — surface them as a
            // fallback so users don't lose their notes pre-
            // migration.
            let offset = (i - current_method_line) as u32;
            if let Some(a) = idx.at_method_line(key, offset) {
                out[i] = Some(a.clone());
            }
            continue;
        }
        let multi_close: Option<&'static str> = if trimmed.starts_with(".array-data ") {
            Some(".end array-data")
        } else if trimmed.starts_with(".packed-switch ") {
            Some(".end packed-switch")
        } else if trimmed.starts_with(".sparse-switch") {
            Some(".end sparse-switch")
        } else {
            None
        };
        // Look up OpIndex first; fall back to MethodLine for
        // records that haven't been upgraded yet. The upgrade
        // pass at bundle-open should clear most of these.
        let this_op = op_cursor;
        if let Some(a) = idx.at_op_index(key, this_op) {
            out[i] = Some(a.clone());
        } else {
            let offset = (i - current_method_line) as u32;
            if let Some(a) = idx.at_method_line(key, offset) {
                out[i] = Some(a.clone());
            }
        }
        op_cursor = op_cursor.saturating_add(1);
        if let Some(closer) = multi_close {
            skip_until = Some(closer);
        }
    }
    out
}
