//! Shell render helpers — palette, progress/loading, tab bar, overflow.
//!
//! Lives in its own module via a second `impl Shell` block so the
//! method bodies don't need rewriting. Each method here remains
//! `pub(crate)` and is callable from the main `Shell` impl in
//! `lib.rs` exactly as before.

use std::sync::{Arc, Mutex};

use gpui::{
    div, list, prelude::*, px, rgb, App, Context, Pixels,
    SharedString,
};

use crate::cfg_block::build_cfg_from_text_sections;
use crate::context_menu::{self, ContextMenuState};
use crate::{
    LeafId, LoadedBundle, Progress, SearchEntry, Shell, ShellState,
};

fn panel_active() -> gpui::Rgba {
    rgb(0x2e2e34)
}
fn panel_inactive() -> gpui::Rgba {
    rgb(0x1e1e22)
}

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
        let results: Vec<SearchEntry> = self.palette_visible_entries();
        let selected = self.palette_selected;
        let results_arc: Arc<Vec<SearchEntry>> = Arc::new(results);
        let scroll = self.palette_list_state.clone();
        let len = self.palette_list_len;
        let weak = cx.entity().downgrade();

        // Annotation-edit chip — shown above the input row when
        // the palette is in edit mode. Takes precedence over the
        // scope chip (they're mutually exclusive in practice).
        let edit_chip: Option<gpui::Div> = self.annotation_edit.as_ref().map(|edit| {
            div()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(border)
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().text_xs().text_color(rgb(0x66c2ff)).child("✎"))
                .child(
                    div()
                        .flex_1()
                        .text_color(fg)
                        .text_sm()
                        .font_family("Courier New")
                        .child(edit.chip_label.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(dim)
                        .child(SharedString::from("Enter to save · Esc to cancel")),
                )
        });

        // Scope chip — shown above the input row when scoped.
        let scope_chip: Option<gpui::Div> = self.palette_scope.as_ref().map(|scope| {
            let progress = scope.progress.as_ref().map(|p| p.lock().clone());
            let count_text = match progress.as_ref() {
                Some(p) => format!(
                    "{} — indexing {}/{}",
                    scope.label, p.current, p.total
                ),
                None => format!("{} — {} results", scope.label, scope.entries.len()),
            };
            div()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .bg(rgb(0x2a313cff & 0x00ff_ffff | 0xff_00_00_00))
                .border_b_1()
                .border_color(border)
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x66c2ff))
                                .child("⇉"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_color(fg)
                                .text_sm()
                                .font_family("Courier New")
                                .child(SharedString::from(count_text)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(dim)
                                .child(SharedString::from("Esc clears scope")),
                        ),
                )
        });

        let status = if self.palette_scope.is_some() {
            format!("{} shown", len)
        } else if self.search_indexing {
            "indexing…".to_string()
        } else if self.search_index.is_none() {
            "no index".to_string()
        } else {
            format!(
                "{} of {} matches",
                len,
                self.search_index.as_ref().map(|i| i.entries.len()).unwrap_or(0)
            )
        };

        // Placeholder reflects what the palette is being used for:
        // an inline annotation edit (the edit chip is already
        // visible above the input), a scoped result list, or
        // bundle-wide search.
        let placeholder = if let Some(edit) = self.annotation_edit.as_ref() {
            match edit.facet {
                crate::AnnotationFacet::Rename => "Type a new name, Enter to save…",
                crate::AnnotationFacet::Comment => "Type a comment, Enter to save…",
            }
        } else if self.palette_scope.is_some() {
            "filter results…"
        } else {
            "search symbols, classes, strings…"
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
                    .child(if self.annotation_edit.is_some() { "✎" } else { "⌕" }),
            )
            .child(
                div()
                    .flex_1()
                    .text_base()
                    .child(self.palette_query.render(fg, dim, placeholder, "Courier New")),
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
                .w_full()
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
            .child({
                let mut card = div()
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
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        |_ev, _w, cx: &mut App| {
                            cx.stop_propagation();
                        },
                    );
                // Mode tab strip — first row of the card. Hidden
                // when an annotation edit is active because the
                // edit chip occupies that visual slot.
                if self.annotation_edit.is_none() {
                    card = card.child(self.render_palette_mode_tabs(border, fg, dim, accent, cx));
                }
                if let Some(chip) = edit_chip {
                    card = card.child(chip);
                } else if let Some(chip) = scope_chip {
                    card = card.child(chip);
                }
                match self.palette_mode {
                    crate::PaletteMode::Text => {
                        card = card.child(input_row);
                        if self.annotation_edit.is_none() {
                            card = card.child(list_el);
                        }
                    }
                    crate::PaletteMode::Binary => {
                        card = card.child(self.render_palette_bin_grammar_tabs(
                            border, fg, dim, cx,
                        ));
                        card = card
                            .child(self.render_palette_bin_input(border, fg, dim, accent, cx));
                        if self.palette_bin_grammar == crate::BinaryGrammar::Asm
                            && self.palette_bin_results.is_none()
                            && !self.palette_asm_candidates.is_empty()
                        {
                            card = card.child(
                                self.render_palette_asm_dropdown(border, fg, dim, accent, cx),
                            );
                        }
                        card = card
                            .child(self.render_palette_bin_results(border, fg, dim, accent, cx));
                    }
                }
                card
            })
    }

    /// Two-tab strip showing the current mode. Click switches.
    /// Keyboard chords (⌘1 / ⌘2) are wired separately.
    fn render_palette_mode_tabs(
        &self,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mode = self.palette_mode;
        let tab = |label: &'static str,
                   shortcut: &'static str,
                   is_active: bool,
                   id: &'static str,
                   on_click: fn(&mut Shell, &mut Context<Shell>)| {
            let bg = if is_active { panel_active() } else { panel_inactive() };
            let text_col = if is_active { fg } else { dim };
            div()
                .id(id)
                .px_4()
                .h(px(28.))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .bg(bg)
                .text_sm()
                .text_color(text_col)
                .cursor_pointer()
                .child(SharedString::from(label))
                .child(div().text_xs().text_color(dim).child(SharedString::from(shortcut)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _ev, _w, cx| {
                        on_click(this, cx);
                    }),
                )
        };
        let _ = accent; // reserved for future highlight; silence unused
        div()
            .h(px(28.))
            .flex()
            .flex_row()
            .border_b_1()
            .border_color(border)
            .child(tab(
                "Symbols & strings",
                "⌘1",
                mode == crate::PaletteMode::Text,
                "palette-mode-text",
                |shell, cx| shell.palette_set_mode_text(cx),
            ))
            .child(tab(
                "Binary",
                "⌘2",
                mode == crate::PaletteMode::Binary,
                "palette-mode-binary",
                |shell, cx| shell.palette_set_mode_binary(cx),
            ))
    }

    fn render_palette_bin_input(
        &self,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let placeholder = match self.palette_bin_grammar {
            crate::BinaryGrammar::Bytes => {
                "Byte pattern: c0 03 5f d6, e? ?? ff *(0..16) c0"
            }
            crate::BinaryGrammar::Asm => "AArch64 asm: mov w0, #1 ; ret",
        };
        let weak = cx.entity().downgrade();
        let code_only_box = crate::checkbox::checkbox(
            "palette-bin-code-only",
            "Code only",
            self.palette_bin_code_only,
            fg,
            dim,
            accent,
            move |cx| {
                if let Some(entity) = weak.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.palette_toggle_bin_code_only(cx);
                    });
                }
            },
        );
        let mut row = div()
            .h(px(40.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .px_3()
            .gap_3()
            .border_b_1()
            .border_color(border)
            .child(div().text_color(dim).text_base().child("⌗"))
            .child(
                div().flex_1().text_base().child(
                    self.palette_bin_query
                        .render(fg, dim, placeholder, "Courier New"),
                ),
            )
            .child(code_only_box);
        if let Some(err) = self.palette_bin_error.as_ref() {
            row = row.child(
                div()
                    .text_xs()
                    .text_color(rgb(0xff6060))
                    .child(SharedString::from(err.clone())),
            );
        }
        row
    }

    /// Small two-tab strip inside Binary mode that toggles between
    /// the byte-mask grammar and the typed-assembly composer.
    fn render_palette_bin_grammar_tabs(
        &self,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let grammar = self.palette_bin_grammar;
        let tab = |label: &'static str,
                   shortcut: Option<&'static str>,
                   is_active: bool,
                   id: &'static str,
                   target: crate::BinaryGrammar| {
            let bg = if is_active { panel_active() } else { panel_inactive() };
            let text_col = if is_active { fg } else { dim };
            let mut d = div()
                .id(id)
                .px_3()
                .h(px(22.))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .bg(bg)
                .text_xs()
                .text_color(text_col)
                .cursor_pointer()
                .child(SharedString::from(label));
            if let Some(s) = shortcut {
                d = d.child(div().text_xs().text_color(dim).child(SharedString::from(s)));
            }
            d.on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _ev, _w, cx| {
                    this.palette_set_bin_grammar(target, cx);
                }),
            )
        };
        div()
            .h(px(22.))
            .flex()
            .flex_row()
            .border_b_1()
            .border_color(border)
            .child(tab(
                "Bytes",
                None,
                grammar == crate::BinaryGrammar::Bytes,
                "palette-bin-grammar-bytes",
                crate::BinaryGrammar::Bytes,
            ))
            .child(tab(
                "Asm",
                Some("⌘B"),
                grammar == crate::BinaryGrammar::Asm,
                "palette-bin-grammar-asm",
                crate::BinaryGrammar::Asm,
            ))
    }

    /// Autocomplete dropdown for asm-mode binary search.
    fn render_palette_asm_dropdown(
        &self,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
        _cx: &mut Context<Self>,
    ) -> gpui::Div {
        let selected = self.palette_asm_selected;
        let rows = self.palette_asm_candidates.iter().enumerate().map(
            |(i, cand)| {
                let is_sel = i == selected;
                let bg = if is_sel { accent } else { rgb(0x00000000) };
                let text_col = if is_sel { fg } else { dim };
                div()
                    .h(px(20.))
                    .px_3()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .bg(bg)
                    .text_sm()
                    .font_family("Courier New")
                    .text_color(text_col)
                    .child(SharedString::from(cand.variant.template.clone()))
            },
        );
        let mut col = div()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(border)
            .max_h(px(260.));
        for r in rows {
            col = col.child(r);
        }
        col
    }

    fn render_palette_bin_results(
        &self,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let _ = border;
        let _ = fg;
        let weak = cx.entity().downgrade();
        let selected = self.palette_selected;
        match self.palette_bin_results.as_ref() {
            None => div()
                .flex_1()
                .p_3()
                .text_sm()
                .text_color(dim)
                .child(SharedString::from(
                    "Enter to run. See docs/BinSearch.md for the pattern grammar.",
                )),
            Some(result) if result.matches.is_empty() => div()
                .flex_1()
                .p_3()
                .text_sm()
                .text_color(dim)
                .child(SharedString::from(format!(
                    "No matches for `{}`",
                    result.pattern
                ))),
            Some(result) => {
                let matches: Arc<Vec<glass_api::BinMatch>> = Arc::new(result.matches.clone());
                let state = self.palette_bin_list_state.clone();
                let header = SharedString::from(format!(
                    "{} of {} matches",
                    result.shown, result.total
                ));
                let row_renderer = {
                    let matches = matches.clone();
                    let weak = weak.clone();
                    move |index: usize, _w: &mut gpui::Window, _cx: &mut gpui::App| {
                        let Some(m) = matches.get(index) else {
                            return div().into_any();
                        };
                        let is_sel = index == selected;
                        let bg = if is_sel { accent } else { rgb(0x00000000) };
                        let weak = weak.clone();
                        let section = SharedString::from(m.section.clone());
                        let address = SharedString::from(m.address.clone());
                        let preview = SharedString::from(m.preview.clone());
                        div()
                            .id(("bin-row", index))
                            .h(px(22.))
                            .flex_shrink_0()
                            .w_full()
                            .px_3()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_4()
                            .bg(bg)
                            .text_sm()
                            .font_family("Courier New")
                            .cursor_pointer()
                            .child(
                                div()
                                    .w(px(140.))
                                    .flex_shrink_0()
                                    .text_color(rgb(0xa0a0a8))
                                    .child(section),
                            )
                            .child(
                                div()
                                    .w(px(160.))
                                    .flex_shrink_0()
                                    .text_color(rgb(0xb0c8ff))
                                    .child(address),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_color(rgb(0xd6d6d6))
                                    .child(preview),
                            )
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_ev, _w, cx: &mut App| {
                                    if let Some(entity) = weak.upgrade() {
                                        cx.update_entity(&entity, |shell, cx| {
                                            shell.palette_selected = index;
                                            shell.palette_bin_activate(cx);
                                        });
                                    }
                                },
                            )
                            .into_any()
                    }
                };
                // Flex-1 + min-h-0 lets the list shrink inside
                // the card's bounded height so its own internal
                // virtualization runs against a finite viewport
                // — without min_h_0 the list refuses to shrink
                // below its preferred (content) height and the
                // viewport ends up as tall as the row count.
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(20.))
                            .flex_shrink_0()
                            .px_3()
                            .pt_1()
                            .text_xs()
                            .text_color(dim)
                            .child(header),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(list(state, row_renderer).size_full()),
                    )
            }
        }
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
        crate::dex_callgraph::seed_root(
            view,
            &bundle.method_calls,
            &bundle.bodies,
            &bundle.method_lines,
            class_jni,
            method_decl,
        );
    }

    pub(crate) fn expand_dex_callee(&mut self, key: &str, cx: &mut Context<Self>) {
        let bundle = match &self.state {
            ShellState::Ready(b) => b.clone(),
            _ => return,
        };
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.dex_callgraph.as_mut() else { return };
        let changed = crate::dex_callgraph::expand_callee(
            view,
            &bundle.method_calls,
            &bundle.bodies,
            &bundle.method_lines,
            key,
        );
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
            // w_full + min_w_0 lets the bar take the parent column's
            // current width and shrink below the children's
            // intrinsic sum on window shrink. Without min_w_0, the
            // flex_shrink_0 children pin the bar at their combined
            // width and the measure canvas never sees the smaller
            // dimension after the user drags the window narrower.
            .w_full()
            .min_w(px(0.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_stretch()
            // Clip overflow visually — the per-frame overflow math
            // hides the tabs that wouldn't fit, but until the next
            // paint settles we may render a frame with everything
            // until the canvas reports new bounds.
            .overflow_hidden()
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
