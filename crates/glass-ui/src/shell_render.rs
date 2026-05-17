//! Shell render helpers — palette, progress/loading, tab bar, overflow.
//!
//! Lives in its own module via a second `impl Shell` block so the
//! method bodies don't need rewriting. Each method here remains
//! `pub(crate)` and is callable from the main `Shell` impl in
//! `lib.rs` exactly as before.

use std::sync::{Arc, Mutex};

use gpui::{
    div, list, prelude::*, px, rgb, App, Context, ListAlignment, ListOffset, ListState, Pixels,
    SharedString, Window,
};

use crate::cfg_block::build_cfg_from_text_sections;
use crate::context_menu::{self, ContextMenuState};
use crate::listing_render::LISTING_ROW_HEIGHT;
use crate::palette::{COLOUR_BB_SEPARATOR, COLOUR_SYMBOL_HEADER};
use crate::scrollbar::list_scrollbar;
use crate::{
    LeafId, LoadedBundle, Progress, SearchEntry, Shell, ShellState, Tab, TabKind, TextTooltip,
};

impl Shell {
    pub(crate) fn render_palette(
        &self,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let results: Vec<SearchEntry> = self
            .search_index
            .as_ref()
            .map(|idx| {
                idx.filter(&self.palette_query, 50)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let selected = self.palette_selected;
        let results_arc: Arc<Vec<SearchEntry>> = Arc::new(results);
        let scroll = self.palette_list_state.clone();
        let len = self.palette_list_len;
        let weak = cx.entity().downgrade();

        let status = if self.search_indexing {
            "indexing…".to_string()
        } else if self.search_index.is_none() {
            "no index".to_string()
        } else {
            format!("{} of {} matches", len, self
                .search_index
                .as_ref()
                .map(|i| i.entries.len())
                .unwrap_or(0))
        };

        let input_row = div()
            .h(px(40.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .px_3()
            .gap_3()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_color(dim)
                    .text_base()
                    .child("⌕"),
            )
            .child(
                div()
                    .flex_1()
                    .text_color(fg)
                    .text_base()
                    .font_family("Courier New")
                    .child(if self.palette_query.is_empty() {
                        SharedString::from("search symbols, classes, strings…")
                    } else {
                        SharedString::from(self.palette_query.clone())
                    }),
            )
            .child(div().text_color(dim).text_xs().child(status));

        let results_arc_for_list = results_arc.clone();
        let list_el = list(scroll, move |index, _w, _cx| {
            let Some(entry) = results_arc_for_list.get(index) else {
                return div().into_any();
            };
            let is_sel = index == selected;
            let bg = if is_sel { accent } else { rgb(0x00000000) };
            let weak = weak.clone();
            div()
                .id(("palette-row", index))
                .h(px(28.))
                .px_3()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .bg(bg)
                .cursor_pointer()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |_ev, _w, cx: &mut App| {
                        if let Some(entity) = weak.upgrade() {
                            cx.update_entity(&entity, |shell, cx| {
                                shell.palette_selected = index;
                                shell.palette_activate(cx);
                            });
                        }
                    },
                )
                .child(
                    div()
                        .w(px(20.))
                        .text_color(if is_sel { rgb(0xffffff) } else { dim })
                        .child(SharedString::from(entry.kind_glyph)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(if is_sel { rgb(0xffffff) } else { fg })
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(SharedString::from(entry.display.clone())),
                )
                .child(
                    div()
                        .max_w(px(280.))
                        .text_xs()
                        .text_color(if is_sel { rgb(0xddddee) } else { dim })
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(SharedString::from(entry.chip.clone())),
                )
                .into_any()
        })
        .flex_1();

        // Backdrop + centered card. Use `rgba()` not `rgb()` — gpui's
        // `rgb()` ignores the alpha byte and reads 0x00000088 as a
        // *blue* colour; `rgba(... aa)` is what we want. `.occlude()`
        // blocks every mouse interaction (click, hover, scroll-wheel)
        // from reaching the window underneath while the modal is up.
        div()
            .absolute()
            .inset_0()
            .bg(gpui::rgba(0x000000bb))
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    // Backdrop click closes.
                    this.close_palette(cx);
                }),
            )
            .child(
                div()
                    .id("palette-card")
                    .mt(px(80.))
                    .w(px(960.))
                    .h(px(540.))
                    .bg(panel)
                    .border_1()
                    .border_color(border)
                    .rounded_md()
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    // Eat clicks inside so the backdrop handler doesn't fire.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        |_ev, _w, cx: &mut App| {
                            cx.stop_propagation();
                        },
                    )
                    .child(input_row)
                    .child(list_el),
            )
    }

    /// Render the right-click context menu as a small floating panel
    /// positioned at the click site. An occluded backdrop covers the
    /// window so clicks outside dismiss the menu without falling
    /// through to whatever's underneath.
    pub(crate) fn render_context_menu(
        &self,
        menu: &ContextMenuState,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        accent: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        context_menu::render_context_menu(menu, panel, border, fg, accent, cx)
    }

    pub(crate) fn render_loading(
        &self,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> impl IntoElement {
        match self.progress.as_ref() {
            Some(p) => self
                .render_progress(p, panel, border, fg, dim, accent)
                .into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(dim)
                .child("Loading…")
                .into_any_element(),
        }
    }

    pub(crate) fn render_progress(
        &self,
        progress: &Arc<Mutex<Progress>>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> impl IntoElement {
        let snapshot: Progress = progress
            .lock()
            .ok()
            .map(|p| p.clone())
            .unwrap_or(Progress {
                label: String::new(),
                phase: SharedString::from("Loading…"),
                current: 0,
                total: 0,
                done: false,
            });

        let phase = snapshot.phase.clone();
        let detail = if snapshot.total > 0 {
            format!("{} / {}", snapshot.current, snapshot.total)
        } else {
            String::new()
        };
        let fraction = if snapshot.total > 0 {
            (snapshot.current as f32 / snapshot.total as f32).clamp(0., 1.)
        } else {
            0.
        };

        // Indeterminate-style placeholder when there's no total yet:
        // show a half-width bar pinned at the start.
        let bar_width_percent = if snapshot.total > 0 {
            fraction * 100.
        } else {
            25.
        };

        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(fg)
                    .child(phase),
            )
            .child(
                // Track
                div()
                    .w(px(360.))
                    .h(px(6.))
                    .bg(panel)
                    .border_1()
                    .border_color(border)
                    .rounded_sm()
                    .relative()
                    .child(
                        // Fill
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .h_full()
                            .bg(accent)
                            .rounded_sm()
                            .w(gpui::relative(bar_width_percent / 100.)),
                    ),
            )
            .child(div().text_xs().text_color(dim).child(detail))
    }

    pub(crate) fn render_two_pane(
        &mut self,
        bundle: LoadedBundle,
        cx: &mut Context<Self>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> gpui::AnyElement {
        crate::two_pane::render_two_pane(self, bundle, cx, panel, border, fg, dim, accent)
    }


    pub(crate) fn render_section_map(
        &mut self,
        bundle: &LoadedBundle,
        artifact: &glass_db::ArtifactId,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        crate::section_map::render_section_map(self, bundle, artifact, panel, border, fg, dim, cx)
    }

    #[allow(dead_code)]

    /// Render the CFG canvas for the function at `entry_addr` in
    /// `artifact`. The graph is built lazily on the first paint; the
    /// blocks are placed in world space (one rank per `BlockLayout.y`
    /// unit, columns at `BlockLayout.x`) and the camera maps them to
    /// screen pixels.
    pub(crate) fn render_cfg(
        &mut self,
        bundle: &LoadedBundle,
        artifact: &glass_db::ArtifactId,
        entry_addr: u64,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        crate::cfg_render::render_cfg(self, bundle, artifact, entry_addr, panel, border, fg, dim, cx)
    }

    /// Render the DEX method call-graph view.
    ///
    /// Each method is a small pill-shaped node showing the method
    /// name; calls between methods become lines. Initial scene is
    /// the root method + its direct callees fanned out around it.
    /// Clicking a callee node expands it by adding *its* direct
    /// callees to the scene.
    pub(crate) fn render_dex_callgraph(
        &mut self,
        bundle: &LoadedBundle,
        class_jni: &str,
        method_decl: &str,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        crate::dex_cg_render::render_dex_callgraph(self, bundle, class_jni, method_decl, panel, border, fg, dim, accent, cx)
    }
    pub(crate) fn ensure_dex_callgraph_built(&mut self, class_jni: &str, method_decl: &str) {
        let Some(idx) = self.active_tab else { return };
        let bundle = match &self.state {
            ShellState::Ready(b) => b.clone(),
            _ => return,
        };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.dex_callgraph.as_mut() else { return };
        crate::dex_callgraph::seed_root(view, &bundle.method_calls, class_jni, method_decl);
    }

    pub(crate) fn expand_dex_callee(&mut self, key: &str, cx: &mut Context<Self>) {
        let bundle = match &self.state {
            ShellState::Ready(b) => b.clone(),
            _ => return,
        };
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.dex_callgraph.as_mut() else { return };
        let changed = crate::dex_callgraph::expand_callee(view, &bundle.method_calls, key);
        cx.notify();
        if changed {
            self.save_state();
        }
    }

    pub(crate) fn dex_cg_pan_by(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.dex_callgraph.as_mut() else { return };
        view.camera.pan_by(dx, dy);
        cx.notify();
    }

    pub(crate) fn dex_cg_zoom_by(
        &mut self,
        anchor: gpui::Point<Pixels>,
        delta: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.dex_callgraph.as_mut() else { return };
        view.camera.zoom_by(anchor, delta);
        cx.notify();
    }

    pub(crate) fn dex_cg_drag_start(&mut self, pos: gpui::Point<Pixels>) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.dex_callgraph.as_mut() else { return };
        view.camera.drag_start(pos);
    }

    pub(crate) fn dex_cg_drag_move(&mut self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.dex_callgraph.as_mut() else { return };
        view.camera.drag_move(pos);
        cx.notify();
    }

    pub(crate) fn dex_cg_drag_end(&mut self) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.dex_callgraph.as_mut() else { return };
        view.camera.drag_end();
    }

    pub(crate) fn ensure_cfg_built(&mut self, artifact: &glass_db::ArtifactId, entry_addr: u64) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        if view.cfg.is_some() {
            return;
        }
        let bundle = match &self.state {
            ShellState::Ready(b) => b,
            _ => return,
        };
        let Some(symbols) = bundle.symbol_maps.get(artifact) else { return };
        let cfg = build_cfg_from_text_sections(
            &bundle.text_sections,
            symbols,
            artifact,
            entry_addr,
        );
        view.cfg = cfg.map(Arc::new);
    }

    pub(crate) fn cfg_pan_by(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        view.camera.pan_by(dx, dy);
        cx.notify();
    }

    pub(crate) fn cfg_zoom_by(
        &mut self,
        anchor: gpui::Point<Pixels>,
        delta: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        view.camera.zoom_by(anchor, delta);
        cx.notify();
    }

    pub(crate) fn cfg_drag_start(&mut self, pos: gpui::Point<Pixels>) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        view.camera.drag_start(pos);
    }

    pub(crate) fn cfg_drag_move(&mut self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        view.camera.drag_move(pos);
        cx.notify();
    }

    pub(crate) fn cfg_drag_end(&mut self) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        view.camera.drag_end();
    }

    pub(crate) fn render_tab_bar(
        &self,
        bundle: &LoadedBundle,
        cx: &mut Context<Self>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> (gpui::AnyElement, gpui::AnyElement) {
        const TAB_WIDTH: f32 = 160.;
        const OVERFLOW_BTN_WIDTH: f32 = 36.;

        let handle = cx.entity().downgrade();
        let active = self.active_tab;
        let tabs = &self.tabs;
        let bar_width = self.tab_bar_width.as_f32();
        let overflow_open = self.overflow_open;

        // How many fixed-width tabs fit. If they all fit, no overflow at all.
        // Otherwise reserve a slot for the overflow button.
        let (visible_count, has_overflow) = if bar_width <= 0. || tabs.is_empty() {
            (tabs.len(), false)
        } else {
            let raw = (bar_width / TAB_WIDTH).floor() as usize;
            if raw >= tabs.len() {
                (tabs.len(), false)
            } else {
                // Slots minus the overflow button.
                let usable = ((bar_width - OVERFLOW_BTN_WIDTH) / TAB_WIDTH).floor() as usize;
                (usable.max(1), true)
            }
        };

        // Decide which tabs are visible. Always include the active one — if it
        // would be hidden, swap it into the last visible slot.
        let mut visible: Vec<usize> = (0..visible_count.min(tabs.len())).collect();
        if has_overflow {
            if let Some(active_idx) = active {
                if !visible.contains(&active_idx) && !visible.is_empty() {
                    let last = visible.len() - 1;
                    visible[last] = active_idx;
                }
            }
        }
        let visible_set: std::collections::HashSet<usize> = visible.iter().copied().collect();
        let hidden: Vec<usize> = (0..tabs.len()).filter(|i| !visible_set.contains(i)).collect();

        // Width-measurement canvas. Its prepaint hook captures bar width into
        // `Shell` so the next render can compute the layout. Sized to fill
        // the bar so its bounds == bar bounds.
        let measure_handle = handle.clone();
        let measure = gpui::canvas(
            move |bounds, _window, cx| {
                if let Some(entity) = measure_handle.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.set_tab_bar_width(bounds.size.width, cx);
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let mut bar = div()
            .h(px(30.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_stretch()
            .border_b_1()
            .border_color(border)
            .bg(panel)
            .relative()
            // Measurement layer underneath the tabs.
            .child(measure);

        if tabs.is_empty() {
            bar = bar.child(
                div()
                    .px_3()
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(dim)
                    .child("Click a class on the left to open a tab"),
            );
        }

        for &index in &visible {
            bar = bar.child(self.render_tab(
                bundle, index, active == Some(index), handle.clone(), panel, border, fg, dim,
                accent,
            ));
        }

        if has_overflow {
            let hidden_count = hidden.len();
            let toggle_handle = handle.clone();
            let overflow_btn = div()
                .h_full()
                .w(px(OVERFLOW_BTN_WIDTH))
                .flex()
                .items_center()
                .justify_center()
                .border_l_1()
                .border_color(border)
                .bg(if overflow_open { rgb(0x36363c) } else { panel })
                .text_color(fg)
                .text_xs()
                .hover(|s| s.bg(rgb(0x36363c)))
                .child(format!("▾ {}", hidden_count))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |_event, _window, cx: &mut App| {
                        let Some(entity) = toggle_handle.upgrade() else { return };
                        cx.update_entity(&entity, |shell, cx| {
                            shell.toggle_overflow(cx);
                        });
                    },
                );
            bar = bar.child(overflow_btn);
        }

        let dropdown: gpui::AnyElement = if overflow_open && !hidden.is_empty() {
            self.render_overflow_dropdown(bundle, &hidden, handle, panel, border, fg, dim, accent)
                .into_any_element()
        } else {
            // Empty placeholder so the caller always has something to attach.
            div().into_any_element()
        };

        (bar.into_any_element(), dropdown)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_tab(
        &self,
        bundle: &LoadedBundle,
        index: usize,
        is_active: bool,
        handle: gpui::WeakEntity<Self>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> impl IntoElement {
        const TAB_WIDTH: f32 = 160.;
        let label = self.tab_display_label(bundle, index);
        let tab_bg = if is_active { accent } else { panel };
        let tab_fg = if is_active { rgb(0xffffff) } else { fg };
        let close_fg = if is_active { rgb(0xffffff) } else { dim };
        let focus_handle = handle.clone();
        let close_handle = handle.clone();

        div()
            .w(px(TAB_WIDTH))
            .h_full()
            .px_3()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .border_r_1()
            .border_color(border)
            .bg(tab_bg)
            .text_color(tab_fg)
            .text_xs()
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(label)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_event, _window, cx: &mut App| {
                            let Some(entity) = focus_handle.upgrade() else { return };
                            cx.update_entity(&entity, |shell, cx| {
                                shell.focus_tab(index, cx);
                            });
                        },
                    ),
            )
            .child(
                div()
                    .w(px(16.))
                    .h(px(16.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .text_color(close_fg)
                    .hover(|s| s.bg(rgb(0x55555c)))
                    .child("×")
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_event, _window, cx: &mut App| {
                            cx.stop_propagation();
                            let Some(entity) = close_handle.upgrade() else { return };
                            cx.update_entity(&entity, |shell, cx| {
                                shell.close_tab(index, cx);
                            });
                        },
                    ),
            )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_overflow_dropdown(
        &self,
        bundle: &LoadedBundle,
        hidden: &[usize],
        handle: gpui::WeakEntity<Self>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        _accent: gpui::Rgba,
    ) -> impl IntoElement {
        let mut menu = div()
            .absolute()
            .top(px(30.))
            .right_0()
            .w(px(280.))
            .max_h(px(400.))
            .overflow_hidden()
            .border_1()
            .border_color(border)
            .bg(panel)
            .shadow_lg()
            .flex()
            .flex_col();

        for &index in hidden {
            let leaf = self.tab_leaf(index);
            let label = self.tab_display_label(bundle, index);
            let origin = leaf
                .and_then(|LeafId(i)| bundle.origins.get(i).cloned())
                .unwrap_or_else(|| SharedString::from(""));

            let focus_handle = handle.clone();
            let close_handle = handle.clone();

            menu = menu.child(
                div()
                    .h(px(28.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(fg)
                    .hover(|s| s.bg(rgb(0x36363c)))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .flex()
                            .gap_2()
                            .child(label)
                            .child(div().text_color(dim).child(origin))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_event, _window, cx: &mut App| {
                                    let Some(entity) = focus_handle.upgrade() else { return };
                                    cx.update_entity(&entity, |shell, cx| {
                                        shell.focus_tab(index, cx);
                                    });
                                },
                            ),
                    )
                    .child(
                        div()
                            .w(px(16.))
                            .h(px(16.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .text_color(dim)
                            .hover(|s| s.bg(rgb(0x55555c)))
                            .child("×")
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_event, _window, cx: &mut App| {
                                    cx.stop_propagation();
                                    let Some(entity) = close_handle.upgrade() else { return };
                                    cx.update_entity(&entity, |shell, cx| {
                                        shell.close_tab(index, cx);
                                    });
                                },
                            ),
                    ),
            );
        }

        menu
    }
}
