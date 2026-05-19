//! Shell state-mutation methods.
//!
//! Constructor, persistence, tab management, palette, context menus,
//! navigation. Lives in a separate file via a sibling `impl Shell`
//! block so the bodies don't need rewriting. All methods are still
//! defined on `Shell` exactly as before.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    px, Bounds, Context, ListAlignment, ListOffset, ListState, Pixels, SharedString, Window,
};

use crate::context_menu::{ContextMenuItem, ContextMenuState};
use crate::hex::{build_hex_rows, hex_row_for_addr};
use crate::listing_model::{build_listing_rows, listing_row_for_addr, DataPeek, ListingRow};
use crate::search::{build_search_index, is_subsequence, SearchJump};
use crate::SearchEntry;
use crate::{
    flatten, scroll_into_view_with_context, Expanded, LeafId,
    LoadedBundle, NativeSectionKind, Progress, RowKind, SectionInfo, Shell, ShellState, Tab,
    TabKind, TextSectionBytes,
};

impl Shell {
    /// Short-lived borrow of the persistence handle, if any. Lets
    /// the load-complete path hydrate annotations without exposing
    /// the field publicly.
    pub(crate) fn db_ref(&self) -> Option<&glass_db::Database> {
        self.db.as_ref()
    }

    pub(crate) fn new(
        path: Option<PathBuf>,
        db: Option<glass_db::Database>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Grab focus immediately so key bindings (cmd-F etc.) work
        // without the user clicking the window first.
        window.focus(&focus_handle, cx);
        let state = if path.is_some() {
            ShellState::Loading
        } else {
            ShellState::Empty
        };
        let source_path = path.clone();
        Self {
            focus_handle,
            source_path,
            state,
            progress: None,
            expanded: Expanded::default(),
            tabs: Vec::new(),
            active_tab: None,
            list_state: ListState::new(0, ListAlignment::Top, px(2000.)),
            visible_count: 0,
            tab_bar_width: px(0.),
            overflow_open: false,
            db,
            section_bar_bounds: Bounds::default(),
            hovered_section: None,
            bar_cursor_addr: None,
            bar_cursor_x: None,
            section_table_scroll: ListState::new(0, ListAlignment::Top, px(2000.)),
            section_table_len: 0,
            search_index: None,
            search_indexing: false,
            palette_open: false,
            palette_query: crate::text_input::TextInput::new(),
            palette_selected: 0,
            palette_list_state: ListState::new(0, ListAlignment::Top, px(2000.)),
            palette_list_len: 0,
            palette_mode: crate::PaletteMode::default(),
            palette_bin_query: crate::text_input::TextInput::new(),
            palette_bin_list_state: ListState::new(0, ListAlignment::Top, px(2000.)),
            palette_bin_code_only: true,
            palette_bin_results: None,
            palette_bin_error: None,
            palette_bin_artifact: None,
            palette_bin_grammar: crate::BinaryGrammar::default(),
            palette_asm_selected: 0,
            palette_asm_candidates: Vec::new(),
            palette_scope: None,
            palette_focused: false,
            context_menu: None,
            about_open: false,
            annotations_pane_open: false,
            annotations_pane_h_offset: px(0.),
            annotation_edit: None,
            colour_picker: None,
            disasm_edit: None,
            hex_edit: None,
            changes_dialog_open: false,
            changes_dialog_confirm_abandon: false,
            export_status: None,
            export_in_progress: false,
            theme: {
                let settings = glass_db::load_window_settings();
                let set = crate::theme::ThemeSet::load();
                Arc::new(set.resolve(settings.theme.as_deref()).clone())
            },
            window_tint: 0,
        }
    }

    pub(crate) fn set_section_bar_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        // Coarse change-detection — avoid notify loops.
        let cur = self.section_bar_bounds;
        let diff = (cur.origin.x - bounds.origin.x).abs()
            + (cur.size.width - bounds.size.width).abs();
        if diff > px(0.5) {
            self.section_bar_bounds = bounds;
            cx.notify();
        }
    }

    pub(crate) fn on_section_bar_move(
        &mut self,
        position: gpui::Point<Pixels>,
        sections: &[SectionInfo],
        cx: &mut Context<Self>,
    ) {
        let bounds = self.section_bar_bounds;
        if bounds.size.width <= px(0.) {
            return;
        }
        let local_x = (position.x - bounds.origin.x).as_f32();
        let width = bounds.size.width.as_f32();
        if local_x < 0. || local_x > width {
            return;
        }
        // Walk sections by accumulated fraction, tracking where each
        // begins so we can interpolate an address within the hit one.
        let mut acc_before = 0.0_f32;
        let target = local_x / width;
        let mut hit: Option<(usize, f32, f32)> = None; // (index, start_frac, frac)
        for (i, sec) in sections.iter().enumerate() {
            let f = sec.fraction.max(0.002);
            let next = acc_before + f;
            if target <= next {
                hit = Some((i, acc_before, f));
                break;
            }
            acc_before = next;
        }
        if hit.is_none() && !sections.is_empty() {
            let last = sections.len() - 1;
            let f = sections[last].fraction.max(0.002);
            hit = Some((last, 1.0 - f, f));
        }
        let (hit_idx, hit_addr) = match hit {
            Some((i, start, f)) => {
                let sec = &sections[i];
                let inner_frac = if f > 0. { (target - start) / f } else { 0. };
                let addr = sec.address + ((sec.size as f32) * inner_frac.clamp(0., 1.)) as u64;
                (Some(i), Some(addr))
            }
            None => (None, None),
        };

        let need_scroll = hit_idx != self.hovered_section;
        if need_scroll
            || self.bar_cursor_addr != hit_addr
            || self.bar_cursor_x != Some(position.x)
        {
            self.hovered_section = hit_idx;
            self.bar_cursor_addr = hit_addr;
            self.bar_cursor_x = Some(position.x);
            if need_scroll {
                if let Some(i) = hit_idx {
                    self.section_table_scroll.scroll_to_reveal_item(i);
                }
            }
            cx.notify();
        }
    }

    /// Clear bar-hover state when the mouse leaves the bar.
    pub(crate) fn on_section_bar_leave(&mut self, cx: &mut Context<Self>) {
        if self.hovered_section.is_some()
            || self.bar_cursor_addr.is_some()
            || self.bar_cursor_x.is_some()
        {
            self.hovered_section = None;
            self.bar_cursor_addr = None;
            self.bar_cursor_x = None;
            cx.notify();
        }
    }

    /// Set the hovered section *without* scrolling the table — used when
    /// the source of the hover is the table itself (rows firing
    /// `on_mouse_move`), so we don't yank the row out from under the
    /// cursor.
    pub(crate) fn set_hovered_section_from_table(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.hovered_section != Some(index)
            || self.bar_cursor_x.is_some()
            || self.bar_cursor_addr.is_some()
        {
            self.hovered_section = Some(index);
            // Clear bar-source cursor data so the renderer's fallback
            // (section centre) kicks in — the table doesn't know a
            // specific address.
            self.bar_cursor_x = None;
            self.bar_cursor_addr = None;
            cx.notify();
        }
    }

    pub(crate) fn ensure_section_table_state(&mut self, len: usize) {
        if self.section_table_len != len {
            self.section_table_scroll =
                ListState::new(len, ListAlignment::Top, px(2000.));
            self.section_table_len = len;
        }
    }

    /// Set the per-bundle window-tint slot (0..=4). Persists on
    /// next flush and triggers a re-render.
    pub(crate) fn set_window_tint(&mut self, slot: u8, cx: &mut Context<Self>) {
        let slot = slot.min(4);
        if self.window_tint == slot {
            return;
        }
        self.window_tint = slot;
        self.save_state();
        cx.notify();
    }

    /// Switch the active theme by name. Persists to
    /// `WindowSettings.theme` and replaces `self.theme` so the next
    /// render uses it.
    pub(crate) fn set_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let set = crate::theme::ThemeSet::load();
        let chosen = set.resolve(Some(name)).clone();
        self.theme = Arc::new(chosen);
        let mut settings = glass_db::load_window_settings();
        settings.theme = Some(name.to_string());
        let _ = glass_db::save_window_settings(&settings);
        cx.notify();
    }

    /// Save the current bundle's UI state to the staged-write set.
    /// The flush timer turns it into a real DB write within 500ms.
    pub(crate) fn save_state(&self) {
        let (Some(db), Some(bundle)) = (&self.db, self.bundle()) else { return };
        let Some(bundle_id) = bundle.bundle_id.clone() else { return };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rec = glass_db::BundleRecord {
            schema_version: 1,
            label: bundle.display_label.clone(),
            last_opened_unix: now,
            artifacts: bundle.artifact_ids.as_ref().clone(),
            open_tabs: self
                .tabs
                .iter()
                .map(|t| {
                    if let (TabKind::Cfg { artifact, entry_addr }, Some(view)) =
                        (&t.kind, t.cfg.as_ref())
                    {
                        return glass_db::TabState::Cfg {
                            artifact: artifact.clone(),
                            entry_addr: *entry_addr,
                            pan_x: view.pan_x(),
                            pan_y: view.pan_y(),
                            zoom: view.zoom(),
                        };
                    }
                    if let (
                        TabKind::DexCallGraph { class_jni, method_decl },
                        Some(view),
                    ) = (&t.kind, t.dex_callgraph.as_ref())
                    {
                        return glass_db::TabState::DexCallGraph {
                            class_jni: class_jni.clone(),
                            method_decl: method_decl.clone(),
                            pan_x: view.pan_x(),
                            pan_y: view.pan_y(),
                            zoom: view.zoom(),
                        };
                    }
                    // Listing / Hex / Smali: capture the scroll
                    // position so reopening returns to where the
                    // user was reading rather than the top.
                    let top_row = t.scroll.logical_scroll_top().item_ix;
                    match &t.kind {
                        TabKind::Listing { artifact, section } => {
                            let scroll_top = t
                                .listing_rows
                                .as_ref()
                                .and_then(|rows| {
                                    rows.get(top_row).and_then(|r| match r {
                                        crate::ListingRow::Instruction { address, .. } => {
                                            Some(*address)
                                        }
                                        _ => None,
                                    })
                                })
                                .unwrap_or(0);
                            return glass_db::TabState::Listing {
                                artifact: artifact.clone(),
                                section: section.clone(),
                                scroll_top,
                            };
                        }
                        TabKind::Hex { artifact, section } => {
                            let scroll_top = t
                                .hex_rows
                                .as_ref()
                                .and_then(|rows| {
                                    rows.get(top_row).and_then(|r| match r {
                                        crate::hex::HexRow::Bytes { address, .. } => {
                                            Some(*address)
                                        }
                                        _ => None,
                                    })
                                })
                                .unwrap_or(0);
                            return glass_db::TabState::Hex {
                                artifact: artifact.clone(),
                                section: section.clone(),
                                scroll_top,
                            };
                        }
                        TabKind::SmaliClass { class_jni } => {
                            return glass_db::TabState::SmaliClass {
                                class_jni: class_jni.clone(),
                                scroll_line: top_row as u32,
                            };
                        }
                        _ => {}
                    }
                    t.kind.to_state()
                })
                .collect(),
            active_tab: self.active_tab,
            expanded_paths: self.expanded.open.iter().cloned().collect(),
            source_path: self
                .source_path
                .as_ref()
                .and_then(|p| p.to_str().map(|s| s.to_string())),
            annotations_pane_open: self.annotations_pane_open,
            window_tint: self.window_tint,
        };
        db.save_bundle(bundle_id, rec);
    }

    /// Restore previously-saved tabs + expansion for this bundle, if any.
    pub(crate) fn restore_state(&mut self, bundle: &LoadedBundle) {
        let (Some(db), Some(bundle_id)) = (&self.db, bundle.bundle_id.as_ref()) else {
            return;
        };
        let rec = match db.load_bundle(bundle_id) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("glass-db read failed: {e:#}");
                return;
            }
        };
        self.annotations_pane_open = rec.annotations_pane_open;
        self.window_tint = rec.window_tint.min(4);
        // Tabs.
        for state in &rec.open_tabs {
            // CFG tabs are persisted with their camera state; restore
            // both the kind and the camera in one step.
            if let glass_db::TabState::Cfg {
                artifact,
                entry_addr,
                pan_x,
                pan_y,
                zoom,
            } = state
            {
                self.tabs.push(Tab::new_cfg_with_camera(
                    artifact.clone(),
                    *entry_addr,
                    *pan_x,
                    *pan_y,
                    *zoom,
                ));
                continue;
            }
            if let glass_db::TabState::DexCallGraph {
                class_jni,
                method_decl,
                pan_x,
                pan_y,
                zoom,
            } = state
            {
                self.tabs.push(Tab::new_dex_callgraph_with_camera(
                    class_jni.clone(),
                    method_decl.clone(),
                    *pan_x,
                    *pan_y,
                    *zoom,
                ));
                continue;
            }
            // Capture both the kind and the persisted scroll
            // anchor (address for listing/hex, line index for
            // smali). Seed the new tab's pending_* field so the
            // first paint scrolls to where the user left off.
            let (kind, pending_addr, pending_line) = match state {
                glass_db::TabState::SmaliClass { class_jni, scroll_line } => (
                    TabKind::SmaliClass { class_jni: class_jni.clone() },
                    None,
                    if *scroll_line == 0 { None } else { Some(*scroll_line as usize) },
                ),
                glass_db::TabState::Listing { artifact, section, scroll_top } => (
                    TabKind::Listing {
                        artifact: artifact.clone(),
                        section: section.clone(),
                    },
                    if *scroll_top == 0 { None } else { Some(*scroll_top) },
                    None,
                ),
                glass_db::TabState::Hex { artifact, section, scroll_top } => (
                    TabKind::Hex {
                        artifact: artifact.clone(),
                        section: section.clone(),
                    },
                    if *scroll_top == 0 { None } else { Some(*scroll_top) },
                    None,
                ),
                glass_db::TabState::SectionMap { artifact } => (
                    TabKind::SectionMap { artifact: artifact.clone() },
                    None,
                    None,
                ),
                // Unknown view kinds (Symbols, Strings, Manifest) are
                // silently dropped until their runtime lands.
                _ => continue,
            };
            // Only restore tabs whose target still exists in this bundle.
            if bundle.resolve(&kind.to_state()).is_some() {
                let mut tab = Tab::new(kind);
                tab.pending_scroll_addr = pending_addr;
                tab.pending_smali_scroll_line = pending_line;
                self.tabs.push(tab);
            }
        }
        if let Some(idx) = rec.active_tab {
            if idx < self.tabs.len() {
                self.active_tab = Some(idx);
            }
        }
        // Expansion. We overwrite any default expansion the caller may
        // have set so the user's last state wins.
        let restored: std::collections::HashSet<Vec<usize>> =
            rec.expanded_paths.into_iter().collect();
        if !restored.is_empty() {
            self.expanded.open = restored;
        }
    }

    pub(crate) fn bundle(&self) -> Option<&LoadedBundle> {
        match &self.state {
            ShellState::Ready(b) => Some(b),
            _ => None,
        }
    }

    /// True when no bundle has been loaded into this window yet. The
    /// Open / Open Recent paths reuse an empty window in preference
    /// to spawning a new one — see `app::open_path`.
    pub(crate) fn is_empty(&self) -> bool {
        matches!(self.state, ShellState::Empty)
    }

    /// Resolve a tab to its current `LeafId` (which may change across
    /// bundle reloads even though the `TabKind` identity is stable).
    pub(crate) fn tab_leaf(&self, index: usize) -> Option<LeafId> {
        let bundle = self.bundle()?;
        let tab = self.tabs.get(index)?;
        bundle.resolve(&tab.kind.to_state())
    }

    pub(crate) fn active_leaf(&self) -> Option<LeafId> {
        self.active_tab.and_then(|i| self.tab_leaf(i))
    }

    /// Tab label as shown in the tab bar.
    ///
    /// We drive the label directly from `TabKind` rather than from
    /// `bundle.labels` so views that don't correspond to a tree leaf
    /// (e.g. a Listing for `.rodata`, opened via the SectionMap) still
    /// have a sensible name. `bundle.labels` is consulted only as a
    /// fallback for SmaliClass when we want the simple class name.
    ///
    /// When multiple tabs share the same `TabKind`, suffix with `#N`.
    pub(crate) fn tab_display_label(&self, bundle: &LoadedBundle, index: usize) -> SharedString {
        let Some(tab) = self.tabs.get(index) else {
            return SharedString::from(format!("#{}", index));
        };
        let base: SharedString = match &tab.kind {
            TabKind::Listing { section, .. } => SharedString::from(section.clone()),
            TabKind::Hex { section, .. } => SharedString::from(section.clone()),
            TabKind::SectionMap { .. } => {
                // SectionMap leaves carry a "<lib> (overview)" label
                // already; fall back to the leaf label when we can.
                self.tab_leaf(index)
                    .and_then(|LeafId(i)| bundle.labels.get(i).cloned())
                    .unwrap_or_else(|| SharedString::from("overview"))
            }
            TabKind::SmaliClass { class_jni } => self
                .tab_leaf(index)
                .and_then(|LeafId(i)| bundle.labels.get(i).cloned())
                .unwrap_or_else(|| SharedString::from(class_jni.clone())),
            TabKind::Manifest => self
                .tab_leaf(index)
                .and_then(|LeafId(i)| bundle.labels.get(i).cloned())
                .unwrap_or_else(|| SharedString::from("manifest")),
            TabKind::Cfg { artifact, entry_addr } => {
                let name = bundle
                    .symbol_maps
                    .get(artifact)
                    .and_then(|sm| sm.at(*entry_addr))
                    .map(|s| s.display_name.clone())
                    .unwrap_or_else(|| format!("sub_{entry_addr:x}"));
                SharedString::from(format!("CFG: {name}"))
            }
            TabKind::DexCallGraph { method_decl, .. } => {
                let name = method_decl
                    .split('(')
                    .next()
                    .unwrap_or(method_decl);
                SharedString::from(format!("Call graph: {name}"))
            }
        };
        // Count tabs of the same kind. Number only when ≥2 exist.
        let total = self.tabs.iter().filter(|t| t.kind == tab.kind).count();
        if total <= 1 {
            return base;
        }
        let nth = 1 + self.tabs[..index].iter().filter(|t| t.kind == tab.kind).count();
        SharedString::from(format!("{base} #{nth}"))
    }

    pub(crate) fn set_tab_bar_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        // Only notify on real change to avoid an infinite re-render loop —
        // canvas writes width → notify → render → canvas writes width → ...
        if (self.tab_bar_width - width).abs() > px(0.5) {
            self.tab_bar_width = width;
            cx.notify();
        }
    }

    pub(crate) fn toggle_overflow(&mut self, cx: &mut Context<Self>) {
        self.overflow_open = !self.overflow_open;
        cx.notify();
    }

    /// Lazily populate the active tab's line cache. Returns `None` if
    /// there is no active tab or the bundle is gone.
    /// Spawn a worker thread that runs `build_listing_rows`, plus a
    /// foreground task that animates progress and installs the result.
    ///
    /// `tab_id` identifies which tab to install rows into. Using the
    /// id (not the `TabKind`) means two tabs with the same kind —
    /// e.g. "Follow in new tab" duplicates a section's Listing —
    /// each get their own rows installed once their worker
    /// completes.
    pub(crate) fn spawn_listing_build(
        &self,
        tab_id: crate::TabId,
        text: TextSectionBytes,
        symbols: Arc<glass_arch_arm64::SymbolMap>,
        data: Arc<DataPeek>,
        progress: Arc<Mutex<Progress>>,
        cx: &mut Context<Self>,
    ) {
        let progress_for_bg = progress.clone();
        let symbols_for_bg = symbols.clone();
        let text_for_bg = text.clone();
        let data_for_bg = data.clone();
        let build_task = cx.background_executor().spawn(async move {
            build_listing_rows(
                &text_for_bg,
                &symbols_for_bg,
                &data_for_bg,
                Some(&progress_for_bg),
            )
        });

        let progress_for_poll = progress.clone();
        cx.spawn(async move |this, cx| {
            // Animate the bar while the worker runs. Same shape as the
            // bundle-loader poll loop.
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                let _ = this.update(cx, |_s, cx| cx.notify());
                let done = progress_for_poll.lock().map(|p| p.done).unwrap_or(true);
                if done {
                    break;
                }
            }
            let rows = build_task.await;
            let comment_count = rows
                .iter()
                .filter(|r| {
                    matches!(r, ListingRow::Instruction { comment, .. } if !comment.is_empty())
                })
                .count();
            tracing::info!(
                "listing build: total_rows={}, comments={}",
                rows.len(),
                comment_count
            );
            let rows = Arc::new(rows);
            let _ = this.update(cx, |shell, cx| {
                let Some(idx) = shell.tabs.iter().position(|t| t.id == tab_id) else {
                    return;
                };
                if let Some(tab) = shell.tabs.get_mut(idx) {
                    tab.scroll =
                        ListState::new(rows.len(), ListAlignment::Top, px(2000.));
                    tab.listing_rows = Some(rows.clone());
                    tab.listing_progress = None;
                    // Apply any pending scroll request now that rows exist.
                    if let Some(addr) = tab.pending_scroll_addr.take() {
                        if let Some(row_idx) = listing_row_for_addr(rows.as_ref(), addr)
                        {
                            scroll_into_view_with_context(&tab.scroll, row_idx);
                            tab.selected_row = Some(row_idx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn ensure_active_tab_lines(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(bundle) = self.bundle().cloned() else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        match &tab.kind {
            // SectionMap renders its own widget — no setup here.
            TabKind::SectionMap { .. } => {}
            // Manifest: rows are precomputed at bundle load. Just
            // size the scroll state once on first activation.
            TabKind::Manifest => {
                let len = bundle.manifest_rows.len();
                if tab.lines.is_none() {
                    tab.scroll = ListState::new(len, ListAlignment::Top, px(2000.));
                    // Reuse `lines` as a "did initial setup" marker —
                    // empty vec is enough.
                    tab.lines = Some(Arc::new(Vec::new()));
                }
            }
            // Hex: cheap to build (one row per 16 bytes), do it inline
            // on first activation.
            TabKind::Hex { artifact, section } => {
                let key = (artifact.clone(), section.clone());
                let Some(data) = bundle.data_sections.get(&key) else {
                    return;
                };
                if tab.hex_rows.is_none() {
                    let empty = glass_arch_arm64::SymbolMap::default();
                    let symbols = bundle.symbol_maps.get(artifact).unwrap_or(&empty);
                    let rows = build_hex_rows(data, symbols);
                    tab.scroll = ListState::new(rows.len(), ListAlignment::Top, px(2000.));
                    tab.hex_rows = Some(Arc::new(rows));
                }
                // Pending scroll-to address (clicked from a Listing's
                // resolved-symbol comment, future feature).
                if let Some(addr) = tab.pending_scroll_addr.take() {
                    if let Some(rows) = tab.hex_rows.as_ref() {
                        if let Some(idx) = hex_row_for_addr(rows.as_ref(), addr) {
                            scroll_into_view_with_context(&tab.scroll, idx);
                            tab.selected_row = Some(idx);
                            tab.selected_byte_addr = Some(addr);
                        }
                    }
                }
            }
            // Listing: kick off a background build the first time the
            // tab is activated. Worker thread fills in `listing_rows`;
            // a foreground poll loop animates the progress bar.
            TabKind::Listing { artifact, section } => {
                let artifact = artifact.clone();
                let section = section.clone();
                let key = (artifact.clone(), section.clone());
                let Some(text) = bundle.text_sections.get(&key).cloned() else {
                    return;
                };
                // First decide what to do based on tab state, *then* drop
                // the borrow before calling spawn_listing_build.
                let mut start_build = None;
                if tab.listing_rows.is_none() && tab.listing_progress.is_none() {
                    let empty = glass_arch_arm64::SymbolMap::default();
                    let symbols_arc: Arc<glass_arch_arm64::SymbolMap> = Arc::new(
                        bundle.symbol_maps.get(&artifact).cloned().unwrap_or(empty),
                    );
                    // Snapshot this artifact's data sections so the
                    // worker can peek string literals when forming
                    // adrp+add comments. Sharing through Arc keeps it
                    // cheap on big binaries.
                    let mut data_sections = Vec::new();
                    let mut section_meta = Vec::new();
                    for ((aid, name), ds) in bundle.data_sections.iter() {
                        if aid != &artifact {
                            continue;
                        }
                        // Skip DWARF / debug sections: they live in
                        // their own base-0 address space (when unlinked
                        // or shipped that way) and trick `peek_string`
                        // into thinking every pointer is "inside" them.
                        if ds.kind == NativeSectionKind::Debug {
                            continue;
                        }
                        if ds.base == 0 {
                            continue;
                        }
                        data_sections.push((ds.base, ds.bytes.clone()));
                        section_meta.push(crate::listing_model::DataSectionMeta {
                            name: name.clone(),
                            base: ds.base,
                            size: ds.bytes.len() as u64,
                        });
                    }
                    // Also include every native section (text + data)
                    // so ADRP targets that land in some other text
                    // page can still resolve to a section name. Text
                    // sections are not in `data_sections` because the
                    // string-peek logic doesn't want them.
                    if let Some(sections) = bundle.native_sections.get(&artifact) {
                        for sec in sections.iter() {
                            if section_meta.iter().any(|m| m.base == sec.address) {
                                continue;
                            }
                            section_meta.push(crate::listing_model::DataSectionMeta {
                                name: sec.name.to_string(),
                                base: sec.address,
                                size: sec.size,
                            });
                        }
                    }
                    let data_arc = Arc::new(DataPeek {
                        sections: data_sections,
                        section_meta,
                    });
                    let n = text.instruction_count();
                    let progress = Arc::new(Mutex::new(Progress {
                        label: section.clone(),
                        phase: SharedString::from("Disassembling…"),
                        current: 0,
                        total: n,
                        done: false,
                    }));
                    tab.listing_progress = Some(progress.clone());
                    let tab_id = tab.id;
                    start_build = Some((tab_id, symbols_arc, data_arc, progress));
                }
                if tab.listing_rows.is_some() {
                    if let Some(addr) = tab.pending_scroll_addr.take() {
                        if let Some(rows) = tab.listing_rows.as_ref() {
                            if let Some(idx) = listing_row_for_addr(rows.as_ref(), addr) {
                                scroll_into_view_with_context(&tab.scroll, idx);
                                tab.selected_row = Some(idx);
                            }
                        }
                    }
                }
                // `tab` borrow ends here; spawn the build outside.
                if let Some((tab_id, symbols_arc, data_arc, progress)) = start_build {
                    self.spawn_listing_build(
                        tab_id, text, symbols_arc, data_arc, progress, cx,
                    );
                }
            }
            // SmaliClass: pre-built line cache.
            TabKind::SmaliClass { .. } => {
                let Some(leaf) = self.tabs.get(active).and_then(|t| {
                    bundle.resolve(&t.kind.to_state())
                }) else {
                    return;
                };
                let tab = self.tabs.get_mut(active).unwrap();
                if tab.lines.is_none() {
                    let lines: Vec<SharedString> = bundle
                        .bodies
                        .get(leaf.0)
                        .map(|s| {
                            s.lines()
                                .map(|l| SharedString::from(l.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    tab.scroll =
                        ListState::new(lines.len(), ListAlignment::Top, px(2000.));
                    tab.lines = Some(Arc::new(lines));
                }
                // Consume any pending deep-link line target now that
                // the body's line count is known (so scroll-to clamps
                // correctly).
                if let Some(line_no) = tab.pending_smali_scroll_line.take() {
                    let len = tab.lines.as_ref().map(|v| v.len()).unwrap_or(0);
                    if line_no < len {
                        scroll_into_view_with_context(&tab.scroll, line_no);
                        tab.selected_row = Some(line_no);
                    }
                }
            }
            // CFG: the data is built lazily on first paint inside
            // render_cfg (it has a borrow of the bundle there); no
            // up-front setup needed here.
            TabKind::Cfg { .. } => {}
            // DexCallGraph: seeded on first paint with the root
            // method + its direct callees.
            TabKind::DexCallGraph { .. } => {}
        }
    }

    pub(crate) fn rebuild_list_state(&mut self) {
        let visible = self
            .bundle()
            .map(|b| flatten(&b.tree, &self.expanded).len())
            .unwrap_or(0);
        if visible != self.visible_count {
            self.list_state = ListState::new(visible, ListAlignment::Top, px(2000.));
            self.visible_count = visible;
        }
    }

    pub(crate) fn toggle_group(&mut self, path: Vec<usize>, cx: &mut Context<Self>) {
        self.expanded.toggle(&path);
        self.rebuild_list_state();

        // On expand: pin the just-expanded group to the top of the viewport
        // so its newly-revealed children flow down into view. ListState's
        // own bottom-clamp keeps short tail expansions from over-scrolling.
        if self.expanded.contains(&path) {
            if let Some(bundle) = self.bundle() {
                let rows = flatten(&bundle.tree, &self.expanded);
                if let Some(group_idx) = rows.iter().position(
                    |r| matches!(&r.kind, RowKind::Group { path: p, .. } if p == &path),
                ) {
                    self.list_state.scroll_to(ListOffset {
                        item_ix: group_idx,
                        offset_in_item: px(0.),
                    });
                }
            }
        }

        cx.notify();
        self.save_state();
    }

    // ---- search palette ----------------------------------------------------

    pub(crate) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.palette_open = false;
        } else {
            self.palette_open = true;
            self.palette_focused = true;
            self.refresh_palette_list();
            self.spawn_search_index_build_if_needed(cx);
            // Default (or refresh) the binary-search artifact to
            // an artifact id that actually has text sections in
            // the loaded bundle. `artifact_ids` lists everything
            // (including DEX artifacts that have no native bytes
            // to scan); using a DEX id here would give zero hits
            // and look like a broken search.
            let valid = match (&self.palette_bin_artifact, self.bundle()) {
                (Some(aid), Some(b)) => {
                    b.text_sections.keys().any(|(x, _)| x == aid)
                        || b.data_sections.keys().any(|(x, _)| x == aid)
                }
                _ => false,
            };
            if !valid {
                self.palette_bin_artifact = self.bundle().and_then(|b| {
                    b.text_sections
                        .keys()
                        .next()
                        .map(|(aid, _)| aid.clone())
                        .or_else(|| {
                            b.data_sections.keys().next().map(|(aid, _)| aid.clone())
                        })
                });
            }
            // Pull keyboard focus onto our root so typing reaches the
            // palette without the user clicking it first.
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    /// Switch palette to text mode (⌘1). State for the other
    /// mode is preserved so toggling back doesn't lose it.
    pub(crate) fn palette_set_mode_text(&mut self, cx: &mut Context<Self>) {
        if self.palette_mode != crate::PaletteMode::Text {
            self.palette_mode = crate::PaletteMode::Text;
            self.palette_selected = 0;
            self.refresh_palette_list();
            cx.notify();
        }
    }

    /// Switch palette to binary mode (⌘2).
    pub(crate) fn palette_set_mode_binary(&mut self, cx: &mut Context<Self>) {
        if self.palette_mode != crate::PaletteMode::Binary {
            self.palette_mode = crate::PaletteMode::Binary;
            self.palette_selected = 0;
            cx.notify();
        }
    }

    /// Toggle the "Code only" filter for bin/insn-search.
    /// Re-runs the last search if results were on screen so the
    /// table updates immediately without the user re-pressing
    /// Enter.
    pub(crate) fn palette_toggle_bin_code_only(&mut self, cx: &mut Context<Self>) {
        self.palette_bin_code_only = !self.palette_bin_code_only;
        if self.palette_bin_results.is_some() && !self.palette_bin_query.text().is_empty() {
            self.run_palette_bin_search(cx);
        }
        cx.notify();
    }

    /// Toggle the binary-mode sub-grammar between Bytes and Asm.
    pub(crate) fn palette_toggle_bin_grammar(&mut self, cx: &mut Context<Self>) {
        self.palette_bin_grammar = match self.palette_bin_grammar {
            crate::BinaryGrammar::Bytes => crate::BinaryGrammar::Asm,
            crate::BinaryGrammar::Asm => crate::BinaryGrammar::Bytes,
        };
        self.palette_bin_results = None;
        self.palette_bin_error = None;
        self.refresh_palette_asm_candidates();
        cx.notify();
    }

    pub(crate) fn palette_set_bin_grammar(
        &mut self,
        grammar: crate::BinaryGrammar,
        cx: &mut Context<Self>,
    ) {
        if self.palette_bin_grammar != grammar {
            self.palette_bin_grammar = grammar;
            self.palette_bin_results = None;
            self.palette_bin_error = None;
            self.refresh_palette_asm_candidates();
            cx.notify();
        }
    }

    /// Rebuild the asm-mode autocomplete candidate list. Cheap —
    /// just walks the variants index. Active instruction = text
    /// after the last `;` in the input.
    pub(crate) fn refresh_palette_asm_candidates(&mut self) {
        if self.palette_bin_grammar != crate::BinaryGrammar::Asm {
            self.palette_asm_candidates.clear();
            self.palette_asm_selected = 0;
            return;
        }
        let active = self
            .palette_bin_query
            .text()
            .rsplit(';')
            .next()
            .unwrap_or("")
            .trim_start();
        self.palette_asm_candidates = glass_api::match_insn_variants(active, 12);
        self.palette_asm_selected = self
            .palette_asm_selected
            .min(self.palette_asm_candidates.len().saturating_sub(1));
    }

    /// Tab inside asm mode: commit the highlighted variant's
    /// template into the current instruction, replacing whatever
    /// the user has half-typed.
    pub(crate) fn palette_asm_commit_template(&mut self, cx: &mut Context<Self>) {
        if self.palette_bin_grammar != crate::BinaryGrammar::Asm {
            return;
        }
        let Some(cand) = self
            .palette_asm_candidates
            .get(self.palette_asm_selected)
            .cloned()
        else {
            return;
        };
        // Split off the current instruction (everything after the
        // last `;`); replace it with the variant's template.
        let mut prefix = String::new();
        let current = self.palette_bin_query.text();
        if let Some(idx) = current.rfind(';') {
            prefix.push_str(&current[..=idx]);
            prefix.push(' ');
        }
        prefix.push_str(&cand.variant.template);
        self.palette_bin_query.set_text(prefix);
        self.palette_bin_results = None;
        self.palette_bin_error = None;
        self.refresh_palette_asm_candidates();
        cx.notify();
    }

    /// Up/Down within the asm dropdown.
    pub(crate) fn palette_asm_move(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.palette_asm_candidates.is_empty() {
            return;
        }
        let max = self.palette_asm_candidates.len() - 1;
        let next =
            (self.palette_asm_selected as i32 + delta).clamp(0, max as i32) as usize;
        if next != self.palette_asm_selected {
            self.palette_asm_selected = next;
            cx.notify();
        }
    }

    pub(crate) fn close_palette(&mut self, cx: &mut Context<Self>) {
        if self.palette_open {
            self.palette_open = false;
            // Reset scope on close so the next cmd-F opens a clean
            // bundle-wide search.
            self.palette_scope = None;
            // Closing the palette also bails any in-progress
            // annotation edit — the editor lives inside it.
            self.annotation_edit = None;
            cx.notify();
        }
    }

    /// Open the palette in scoped mode. The header chip shows
    /// `label`, the list shows `scope.entries` (refined by typing).
    /// Esc clears the scope rather than closing the palette
    /// outright.
    pub(crate) fn open_scoped_palette(
        &mut self,
        scope: crate::PaletteScope,
        cx: &mut Context<Self>,
    ) {
        let was_pending = scope.progress.is_some();
        self.palette_scope = Some(scope);
        self.palette_open = true;
        self.palette_focused = true;
        self.palette_query.clear();
        self.palette_selected = 0;
        self.refresh_palette_list();
        cx.notify();
        if was_pending {
            self.spawn_xref_scope_poller(cx);
        }
    }

    /// Tick at ~30 fps while the open scoped palette is still
    /// waiting on a building xref index. Each tick re-checks the
    /// XrefStore and rebuilds the scope's entries when the index
    /// transitions to Ready. Stops once the scope is gone, closed,
    /// or the index is done.
    fn spawn_xref_scope_poller(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                let stop = this
                    .update(cx, |shell, cx| {
                        shell.refresh_xref_scope_if_pending(cx)
                    })
                    .unwrap_or(true);
                if stop {
                    return;
                }
            }
        })
        .detach();
    }

    /// If the current palette scope is waiting on a building xref
    /// index, peek at the underlying state and rebuild entries when
    /// the index has become Ready. Returns true when the poller
    /// should stop.
    pub(crate) fn refresh_xref_scope_if_pending(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::xref::{PaletteScopeSource, XrefIndexState};
        if !self.palette_open {
            return true;
        }
        let Some(scope) = self.palette_scope.as_ref() else { return true };
        if scope.progress.is_none() {
            return true;
        }
        // Nudge the renderer so the progress meter advances even if
        // no transition is happening yet.
        cx.notify();
        let bundle = match self.bundle().cloned() {
            Some(b) => b,
            None => return true,
        };
        let source = scope.source.clone();
        // Pull the relevant slot. Returns (Some(rebuilt_entries),
        // _) on transition to Ready, (None, false) while still
        // building, (None, true) on Failed.
        let (new_entries, failed) = match source {
            PaletteScopeSource::NativeXrefs { artifact, target_addr } => {
                match bundle.xrefs.native.read().clone() {
                    XrefIndexState::Ready(idx) => (
                        Some(build_native_xref_entries(
                            &bundle,
                            &artifact,
                            target_addr,
                            &idx,
                        )),
                        false,
                    ),
                    XrefIndexState::Failed(_) => (None, true),
                    _ => (None, false),
                }
            }
            PaletteScopeSource::DexCallers { method_key } => {
                match bundle.xrefs.dex_callers.read().clone() {
                    XrefIndexState::Ready(idx) => (
                        Some(build_dex_caller_entries(&bundle, &method_key, &idx)),
                        false,
                    ),
                    XrefIndexState::Failed(_) => (None, true),
                    _ => (None, false),
                }
            }
            PaletteScopeSource::DexFieldRefs { field_ref } => {
                match bundle.xrefs.dex_field_refs.read().clone() {
                    XrefIndexState::Ready(idx) => (
                        Some(build_dex_field_entries(&bundle, &field_ref, &idx)),
                        false,
                    ),
                    XrefIndexState::Failed(_) => (None, true),
                    _ => (None, false),
                }
            }
        };
        if let Some(entries) = new_entries {
            if let Some(scope) = self.palette_scope.as_mut() {
                scope.entries = Arc::new(entries);
                scope.progress = None;
            }
            self.refresh_palette_list();
            cx.notify();
            return true;
        }
        if failed {
            if let Some(scope) = self.palette_scope.as_mut() {
                scope.progress = None;
            }
            cx.notify();
            return true;
        }
        false
    }

    /// Clear the current scope without closing the palette. Used as
    /// the first effect of Esc when scoped — Esc on a non-scoped
    /// palette closes it.
    pub(crate) fn clear_palette_scope(&mut self, cx: &mut Context<Self>) {
        if self.palette_scope.is_some() {
            self.palette_scope = None;
            self.palette_query.clear();
            self.palette_selected = 0;
            self.refresh_palette_list();
            cx.notify();
        }
    }

    /// Right-click handler invoked from a Listing row. Offers Show
    /// CFG + Callers of function when the row is inside a known
    /// symbol; a generic References to address otherwise.
    pub(crate) fn open_listing_context_menu(
        &mut self,
        artifact: glass_db::ArtifactId,
        addr: u64,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle() else { return };
        let covering = bundle
            .symbol_maps
            .get(&artifact)
            .and_then(|sm| sm.covering(addr));
        let mut items = Vec::new();
        // 1) Top items depend on what kind of thing the click
        //    landed on:
        //    - Function symbol → Show CFG + Callers of function
        //    - Object (data) symbol → References to <name>
        //    - No covering symbol → References to 0x<addr>
        match covering {
            Some(sym) if matches!(sym.kind, glass_arch_arm64::SymbolKind::Function) => {
                let label = SharedString::from(sym.display_name.clone());
                let entry_addr = sym.address;
                items.push(ContextMenuItem::ShowCfg {
                    artifact: artifact.clone(),
                    entry_addr,
                    label: label.clone(),
                });
                items.push(ContextMenuItem::CallersOfFunction {
                    artifact: artifact.clone(),
                    entry_addr,
                    label,
                });
            }
            Some(sym) => {
                // Data symbol — xrefs scoped to the symbol's
                // entry address so e.g. ADRP+ADD pairs pointing
                // at this string show up.
                items.push(ContextMenuItem::XrefsToAddress {
                    artifact: artifact.clone(),
                    addr: sym.address,
                    label: SharedString::from(sym.display_name.clone()),
                });
            }
            None => {
                // No covering symbol — but if the click landed
                // inside a recognisable data item (e.g. a C string
                // in `__cstring` with no symtab entry), use the
                // item's start address so the xref query matches
                // the address recorded by ADRP+ADD resolution.
                let (query_addr, label) = match crate::listing_render::item_extent_for(
                    bundle,
                    &artifact,
                    addr,
                ) {
                    Some((start, _end)) if start != addr => {
                        // Show a short string preview when it's a
                        // strings-section item the user clicked
                        // into the middle of.
                        let preview = preview_string_at(bundle, &artifact, start);
                        let label_text = match preview {
                            Some(s) => format!("\"{s}\""),
                            None => format!("0x{start:x}"),
                        };
                        (start, SharedString::from(label_text))
                    }
                    Some((start, _end)) => {
                        let preview = preview_string_at(bundle, &artifact, start);
                        let label_text = match preview {
                            Some(s) => format!("\"{s}\""),
                            None => format!("0x{start:x}"),
                        };
                        (start, SharedString::from(label_text))
                    }
                    None => (addr, SharedString::from(format!("0x{addr:x}"))),
                };
                items.push(ContextMenuItem::XrefsToAddress {
                    artifact: artifact.clone(),
                    addr: query_addr,
                    label,
                });
            }
        }
        // 2) Annotation items. Always address-keyed: the user
        //    right-clicked a specific row, so that row is the
        //    intent. Function-level tagging is still possible —
        //    just right-click the function's entry row (its
        //    address is the same one the SymbolHeader covers).
        let (annot_key, annot_label) =
            (glass_db::AnnotationKey::Address(addr), format!("0x{addr:x}"));
        let existing = bundle
            .annotations
            .get(&artifact)
            .and_then(|idx| match &annot_key {
                glass_db::AnnotationKey::Address(a) => idx.at_address(*a),
                glass_db::AnnotationKey::Symbol(s) => idx.at_symbol(s),
                glass_db::AnnotationKey::Class(c) => idx.at_class(c),
                glass_db::AnnotationKey::Method(c, m) => {
                    idx.at_method(&format!("{c}->{m}"))
                }
                glass_db::AnnotationKey::MethodLine(c, m, line) => {
                    idx.at_method_line(&format!("{c}->{m}"), *line)
                }
            })
            .cloned()
            .unwrap_or_default();
        let comment_label = if existing.comment.is_some() {
            "Edit comment…"
        } else {
            "Add comment…"
        };
        items.push(ContextMenuItem::EditComment {
            artifact: artifact.clone(),
            key: annot_key.clone(),
            current: existing.comment.clone().unwrap_or_default(),
            label: SharedString::from(comment_label),
        });
        items.push(ContextMenuItem::PickColour {
            artifact: artifact.clone(),
            key: annot_key.clone(),
            current: existing.colour,
            label: SharedString::from("Set colour…"),
        });
        // 3) Revert staged disasm edit, if any.
        let has_edit = bundle.edits.get(&artifact, addr).is_some();
        if has_edit {
            items.push(ContextMenuItem::RevertDisasmEdit {
                artifact: artifact.clone(),
                vaddr: addr,
                label: SharedString::from(format!("Revert change ({annot_label})")),
            });
        }
        if !existing.is_empty() {
            items.push(ContextMenuItem::ClearAnnotation {
                artifact,
                key: annot_key,
                label: SharedString::from(format!("Clear annotation ({annot_label})")),
            });
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on a line in a SmaliClass listing → context menu
    /// offering "Show call graph" for the method that contains the
    /// line. The caller determined the method by scanning upward.
    pub(crate) fn open_smali_context_menu(
        &mut self,
        class_jni: String,
        method_decl: String,
        line_offset: u32,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Display name: just the method name (no signature) to keep
        // the menu line readable.
        let display = method_decl
            .split('(')
            .next()
            .unwrap_or(&method_decl)
            .to_string();
        let label = SharedString::from(display.clone());
        let method_key = format!("{class_jni}->{method_decl}");
        // For annotation lookup we need an artifact id. DEX
        // artifacts share the bundle's first DEX artifact id; pick
        // the first one in the bundle's artifact list as the
        // canonical DEX target.
        let dex_artifact = self
            .bundle()
            .and_then(|b| b.artifact_ids.first().cloned());
        let mut items = vec![
            ContextMenuItem::ShowDexCallGraph {
                class_jni: class_jni.clone(),
                method_decl: method_decl.clone(),
                label: label.clone(),
            },
            ContextMenuItem::CallersOfMethod {
                method_key: method_key.clone(),
                label: label.clone(),
            },
        ];
        if let Some(artifact) = dex_artifact {
            // MethodLine keys carry the line offset relative to
            // the `.method` line — line_offset == 0 targets the
            // header itself (the natural fallback for native
            // methods, which have no body).
            let key = glass_db::AnnotationKey::MethodLine(
                class_jni.clone(),
                method_decl.clone(),
                line_offset,
            );
            let existing = self
                .bundle()
                .and_then(|b| b.annotations.get(&artifact))
                .and_then(|idx| idx.at_method_line(&method_key, line_offset))
                .cloned()
                .unwrap_or_default();
            let comment_label = if existing.comment.is_some() {
                "Edit comment…"
            } else {
                "Add comment…"
            };
            // Disambiguate the menu label so a user with several
            // annotations in the same method can see which line
            // they're editing.
            let line_chip = if line_offset == 0 {
                String::new()
            } else {
                format!(" (line {line_offset})")
            };
            items.push(ContextMenuItem::EditComment {
                artifact: artifact.clone(),
                key: key.clone(),
                current: existing.comment.clone().unwrap_or_default(),
                label: SharedString::from(format!("{comment_label}{line_chip}")),
            });
            items.push(ContextMenuItem::PickColour {
                artifact: artifact.clone(),
                key: key.clone(),
                current: existing.colour,
                label: SharedString::from(format!("Set colour…{line_chip}")),
            });
            if !existing.is_empty() {
                let clear_label = if line_offset == 0 {
                    format!("Clear annotation ({display})")
                } else {
                    format!("Clear annotation ({display} line {line_offset})")
                };
                items.push(ContextMenuItem::ClearAnnotation {
                    artifact,
                    key,
                    label: SharedString::from(clear_label),
                });
            }
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on a `.class` header in the smali viewer. Same
    /// annotation surface as `open_smali_context_menu`, keyed on
    /// the class JNI rather than a method.
    pub(crate) fn open_smali_class_context_menu(
        &mut self,
        class_jni: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let dex_artifact = self
            .bundle()
            .and_then(|b| b.artifact_ids.first().cloned());
        let Some(artifact) = dex_artifact else {
            return;
        };
        // Display name: dotted Java form for menu chip
        // ("com.example.Foo") rather than the JNI form
        // ("Lcom/example/Foo;").
        let display = class_jni
            .trim_start_matches('L')
            .trim_end_matches(';')
            .replace('/', ".");
        let label = SharedString::from(display);
        let key = glass_db::AnnotationKey::Class(class_jni.clone());
        let existing = self
            .bundle()
            .and_then(|b| b.annotations.get(&artifact))
            .and_then(|idx| idx.at_class(&class_jni))
            .cloned()
            .unwrap_or_default();
        let comment_label = if existing.comment.is_some() {
            "Edit comment…"
        } else {
            "Add comment…"
        };
        let mut items = vec![
            ContextMenuItem::EditComment {
                artifact: artifact.clone(),
                key: key.clone(),
                current: existing.comment.clone().unwrap_or_default(),
                label: SharedString::from(comment_label),
            },
            ContextMenuItem::PickColour {
                artifact: artifact.clone(),
                key: key.clone(),
                current: existing.colour,
                label: SharedString::from("Set colour…"),
            },
        ];
        if !existing.is_empty() {
            items.push(ContextMenuItem::ClearAnnotation {
                artifact,
                key,
                label: SharedString::from(format!("Clear annotation ({label})")),
            });
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on an address link inside a Listing row. Offers
    /// Follow / Follow in new tab (matching left-click + shift-click
    /// behaviour), plus Show CFG when the target lands in a text
    /// section with a known covering function.
    pub(crate) fn open_link_context_menu(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        is_data: bool,
        display: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        use crate::context_menu::FollowTarget;
        let label = SharedString::from(display);
        let target = if is_data {
            FollowTarget::Hex {
                artifact: artifact.clone(),
                section: section.clone(),
                addr,
            }
        } else {
            FollowTarget::Listing {
                artifact: artifact.clone(),
                section: section.clone(),
                addr,
            }
        };
        let mut items = vec![
            ContextMenuItem::Follow { target: target.clone(), label: label.clone() },
            ContextMenuItem::FollowInNewTab { target, label: label.clone() },
        ];
        // Add Show CFG + Callers of function when the address has a
        // covering function in a text section; otherwise add a
        // generic References to address item.
        if !is_data {
            if let Some(bundle) = self.bundle() {
                if let Some(sym) = bundle
                    .symbol_maps
                    .get(&artifact)
                    .and_then(|sm| sm.covering(addr))
                {
                    items.push(ContextMenuItem::ShowCfg {
                        artifact: artifact.clone(),
                        entry_addr: sym.address,
                        label: SharedString::from(sym.display_name.clone()),
                    });
                    items.push(ContextMenuItem::CallersOfFunction {
                        artifact: artifact.clone(),
                        entry_addr: sym.address,
                        label: SharedString::from(sym.display_name.clone()),
                    });
                } else {
                    items.push(ContextMenuItem::XrefsToAddress {
                        artifact: artifact.clone(),
                        addr,
                        label: label.clone(),
                    });
                }
            }
        } else {
            // Hex target — references to that byte (often a string
            // literal or data pointer).
            items.push(ContextMenuItem::XrefsToAddress {
                artifact: artifact.clone(),
                addr,
                label: label.clone(),
            });
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on a DEX call-graph node. Shows Follow / Follow
    /// in new tab; both navigate to the method's smali. (Smali tabs
    /// dedupe by class so "new tab" reuses an existing class tab —
    /// see the comment in `activate_follow`.)
    /// Right-click on a `.field` line in a smali listing — shows
    /// "References to field" only. (Fields have no follow target;
    /// they're just storage locations.)
    pub(crate) fn open_field_context_menu(
        &mut self,
        field_ref: String,
        display: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let label = SharedString::from(display);
        let items = vec![ContextMenuItem::RefsToField { field_ref, label }];
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    pub(crate) fn open_smali_link_context_menu(
        &mut self,
        leaf: LeafId,
        line: usize,
        method_key: Option<String>,
        display: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        use crate::context_menu::FollowTarget;
        let label = SharedString::from(display);
        let target = FollowTarget::SmaliMethod { leaf, line };
        let mut items = vec![
            ContextMenuItem::Follow { target: target.clone(), label: label.clone() },
            ContextMenuItem::FollowInNewTab { target, label: label.clone() },
        ];
        if let Some(key) = method_key {
            items.push(ContextMenuItem::CallersOfMethod { method_key: key, label });
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    pub(crate) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.is_some() {
            self.context_menu = None;
            cx.notify();
        }
    }

    pub(crate) fn activate_context_menu_item(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.context_menu.as_ref() else { return };
        let Some(item) = menu.items.get(index).cloned() else { return };
        self.context_menu = None;
        match item {
            ContextMenuItem::Follow { target, .. } => {
                self.activate_follow(target, false, cx);
            }
            ContextMenuItem::FollowInNewTab { target, .. } => {
                self.activate_follow(target, true, cx);
            }
            ContextMenuItem::ShowCfg {
                artifact,
                entry_addr,
                label,
            } => {
                self.show_cfg(artifact, entry_addr, label, cx);
            }
            ContextMenuItem::ShowDexCallGraph {
                class_jni,
                method_decl,
                label,
            } => {
                self.show_dex_callgraph(class_jni, method_decl, label, cx);
            }
            ContextMenuItem::XrefsToAddress { artifact, addr, label } => {
                self.open_xrefs_to_address(artifact, addr, label, cx);
            }
            ContextMenuItem::CallersOfFunction { artifact, entry_addr, label } => {
                self.open_xrefs_to_address(artifact, entry_addr, label, cx);
            }
            ContextMenuItem::CallersOfMethod { method_key, label } => {
                self.open_callers_of_method(method_key, label, cx);
            }
            ContextMenuItem::RefsToField { field_ref, label } => {
                self.open_refs_to_field(field_ref, label, cx);
            }
            ContextMenuItem::EditRename { artifact, key, current, .. } => {
                self.begin_annotation_edit(
                    artifact,
                    key,
                    crate::AnnotationFacet::Rename,
                    current,
                    cx,
                );
            }
            ContextMenuItem::EditComment { artifact, key, current, .. } => {
                self.begin_annotation_edit(
                    artifact,
                    key,
                    crate::AnnotationFacet::Comment,
                    current,
                    cx,
                );
            }
            ContextMenuItem::PickColour { artifact, key, current, .. } => {
                self.open_colour_picker(artifact, key, current, cx);
            }
            ContextMenuItem::ClearAnnotation { artifact, key, .. } => {
                self.clear_annotation_at(artifact, key, cx);
            }
            ContextMenuItem::RevertDisasmEdit { artifact, vaddr, .. } => {
                self.revert_disasm_edit(artifact, vaddr, cx);
            }
        }
    }

    /// Stash a pending annotation edit and open the palette in
    /// editor mode. The palette's text input is reused as the
    /// editor: query starts equal to `current`, Enter commits.
    pub(crate) fn begin_annotation_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        facet: crate::AnnotationFacet,
        current: String,
        cx: &mut Context<Self>,
    ) {
        let key_label = match &key {
            glass_db::AnnotationKey::Address(a) => format!("0x{a:x}"),
            glass_db::AnnotationKey::Symbol(s) => s.clone(),
            glass_db::AnnotationKey::Class(c) => c.clone(),
            glass_db::AnnotationKey::Method(c, m) => format!("{c}->{m}"),
            glass_db::AnnotationKey::MethodLine(c, m, line) => {
                format!("{c}->{m}#{line}")
            }
        };
        let chip = match facet {
            crate::AnnotationFacet::Rename => format!("Rename {key_label}"),
            crate::AnnotationFacet::Comment => format!("Comment on {key_label}"),
        };
        self.annotation_edit = Some(crate::AnnotationEdit {
            artifact,
            key,
            facet,
            chip_label: SharedString::from(chip),
        });
        self.palette_open = true;
        self.palette_query.set_text(current);
        self.palette_selected = 0;
        self.palette_list_len = 0;
        self.palette_focused = true;
        cx.notify();
    }

    /// Commit the in-progress annotation edit (called on Enter
    /// while `annotation_edit` is set). Writes through glass-api,
    /// refreshes the in-memory index, opens the pane on success.
    pub(crate) fn commit_annotation_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.annotation_edit.take() else { return };
        let value = self.palette_query.text().to_string();
        self.palette_query.clear();
        self.palette_open = false;
        self.palette_focused = false;
        let result = match edit.facet {
            crate::AnnotationFacet::Rename => {
                self.write_annotation(edit.artifact.clone(), edit.key.clone(), |a| {
                    if value.is_empty() {
                        a.rename = None;
                    } else {
                        a.rename = Some(value.clone());
                    }
                })
            }
            crate::AnnotationFacet::Comment => {
                self.write_annotation(edit.artifact.clone(), edit.key.clone(), |a| {
                    if value.is_empty() {
                        a.comment = None;
                    } else {
                        a.comment = Some(value.clone());
                    }
                })
            }
        };
        if let Err(e) = result {
            tracing::warn!("annotation edit failed: {e:#}");
        }
        cx.notify();
    }

    /// Bail out of an in-progress edit without writing.
    pub(crate) fn cancel_annotation_edit(&mut self, cx: &mut Context<Self>) {
        if self.annotation_edit.is_some() {
            self.annotation_edit = None;
            self.palette_open = false;
            self.palette_focused = false;
            self.palette_query.clear();
            cx.notify();
        }
    }

    pub(crate) fn open_colour_picker(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        current: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        // Anchor the popover near the previous context menu
        // position so it appears under the user's mouse.
        let position = self
            .context_menu
            .as_ref()
            .map(|m| m.position)
            .unwrap_or(gpui::Point {
                x: gpui::px(200.),
                y: gpui::px(200.),
            });
        self.colour_picker = Some(crate::ColourPickerState {
            artifact,
            key,
            position,
            current,
        });
        cx.notify();
    }

    pub(crate) fn close_colour_picker(&mut self, cx: &mut Context<Self>) {
        if self.colour_picker.is_some() {
            self.colour_picker = None;
            cx.notify();
        }
    }

    /// Activator for a swatch click in the colour picker. `rgba ==
    /// None` means "clear the colour facet".
    pub(crate) fn pick_colour(&mut self, rgba: Option<u32>, cx: &mut Context<Self>) {
        let Some(picker) = self.colour_picker.take() else { return };
        let result = self.write_annotation(picker.artifact, picker.key, |a| {
            a.colour = rgba;
        });
        if let Err(e) = result {
            tracing::warn!("colour pick failed: {e:#}");
        }
        cx.notify();
    }

    pub(crate) fn clear_annotation_at(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        cx: &mut Context<Self>,
    ) {
        let result = self.clear_annotation_full(artifact, key);
        if let Err(e) = result {
            tracing::warn!("clear annotation failed: {e:#}");
        }
        cx.notify();
    }

    /// Build a scoped palette for "References to address" / "Callers
    /// of function". Consults the bundle's native xref index; when
    /// the index is still building we open an empty palette and the
    /// chip's progress meter populates. When ready, we resolve each
    /// caller-site address to a `SearchEntry` so the user can jump
    /// directly to it.
    pub(crate) fn open_xrefs_to_address(
        &mut self,
        artifact: glass_db::ArtifactId,
        addr: u64,
        label: SharedString,
        cx: &mut Context<Self>,
    ) {
        use crate::xref::{PaletteScope, PaletteScopeSource, XrefIndexState};
        let Some(bundle) = self.bundle().cloned() else { return };
        let state = bundle.xrefs.native.read().clone();
        let (entries, progress) = match state {
            XrefIndexState::Ready(idx) => {
                let entries = build_native_xref_entries(&bundle, &artifact, addr, &idx);
                (entries, None)
            }
            XrefIndexState::Building(p) => (Vec::new(), Some(p)),
            _ => (Vec::new(), None),
        };
        self.open_scoped_palette(
            PaletteScope {
                label: format!("References to {}", label),
                entries: Arc::new(entries),
                progress,
                source: PaletteScopeSource::NativeXrefs {
                    artifact,
                    target_addr: addr,
                },
            },
            cx,
        );
    }

    /// "Callers of method" — invert the DEX caller index for
    /// `method_key` and turn the caller list into smali deep-link
    /// SearchEntries.
    pub(crate) fn open_callers_of_method(
        &mut self,
        method_key: String,
        label: SharedString,
        cx: &mut Context<Self>,
    ) {
        use crate::xref::{PaletteScope, PaletteScopeSource, XrefIndexState};
        let Some(bundle) = self.bundle().cloned() else { return };
        let state = bundle.xrefs.dex_callers.read().clone();
        let (entries, progress) = match state {
            XrefIndexState::Ready(idx) => {
                let entries = build_dex_caller_entries(&bundle, &method_key, &idx);
                (entries, None)
            }
            XrefIndexState::Building(p) => (Vec::new(), Some(p)),
            _ => (Vec::new(), None),
        };
        self.open_scoped_palette(
            PaletteScope {
                label: format!("Callers of {}", label),
                entries: Arc::new(entries),
                progress,
                source: PaletteScopeSource::DexCallers {
                    method_key,
                },
            },
            cx,
        );
    }

    /// "References to field" — same shape, queries the DEX field-
    /// reference index.
    pub(crate) fn open_refs_to_field(
        &mut self,
        field_ref: String,
        label: SharedString,
        cx: &mut Context<Self>,
    ) {
        use crate::xref::{PaletteScope, PaletteScopeSource, XrefIndexState};
        let Some(bundle) = self.bundle().cloned() else { return };
        let state = bundle.xrefs.dex_field_refs.read().clone();
        let (entries, progress) = match state {
            XrefIndexState::Ready(idx) => {
                let entries = build_dex_field_entries(&bundle, &field_ref, &idx);
                (entries, None)
            }
            XrefIndexState::Building(p) => (Vec::new(), Some(p)),
            _ => (Vec::new(), None),
        };
        self.open_scoped_palette(
            PaletteScope {
                label: format!("References to {}", label),
                entries: Arc::new(entries),
                progress,
                source: PaletteScopeSource::DexFieldRefs {
                    field_ref,
                },
            },
            cx,
        );
    }

    /// Dispatch a Follow / FollowInNewTab action. Plain follow reuses
    /// an existing same-type tab; `new_tab = true` always pushes a
    /// fresh tab.
    pub(crate) fn activate_follow(
        &mut self,
        target: crate::context_menu::FollowTarget,
        new_tab: bool,
        cx: &mut Context<Self>,
    ) {
        use crate::context_menu::FollowTarget;
        match target {
            FollowTarget::Listing { artifact, section, addr } => {
                if new_tab {
                    self.open_listing_force_new_tab(artifact, section, addr, cx);
                } else {
                    self.open_listing_at(artifact, section, addr, cx);
                }
            }
            FollowTarget::Hex { artifact, section, addr } => {
                if new_tab {
                    self.open_hex_force_new_tab(artifact, section, addr, cx);
                } else {
                    self.open_hex_in_new_tab(artifact, section, addr, cx);
                }
            }
            FollowTarget::SmaliMethod { leaf, line } => {
                // Smali tabs always dedupe by class (one tab per
                // class makes sense). new_tab is a no-op here — we
                // honour the request to navigate but won't spawn a
                // duplicate smali tab for the same class.
                let _ = new_tab;
                self.goto_smali_method(leaf, line, cx);
            }
        }
    }

    /// Open (or focus an existing) CFG tab for a function. The CFG
    /// data itself is built lazily on the first paint, so opening a
    /// huge function is cheap up-front.
    pub(crate) fn show_cfg(
        &mut self,
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
        _label: SharedString,
        cx: &mut Context<Self>,
    ) {
        let kind = TabKind::Cfg { artifact, entry_addr };
        if let Some(i) = self.tabs.iter().position(|t| t.kind == kind) {
            self.active_tab = Some(i);
        } else {
            self.tabs.push(Tab::new(kind));
            self.active_tab = Some(self.tabs.len() - 1);
        }
        self.overflow_open = false;
        cx.notify();
        self.save_state();
    }

    /// Open (or focus an existing) DEX call-graph tab.
    pub(crate) fn show_dex_callgraph(
        &mut self,
        class_jni: String,
        method_decl: String,
        _label: SharedString,
        cx: &mut Context<Self>,
    ) {
        let kind = TabKind::DexCallGraph { class_jni, method_decl };
        if let Some(i) = self.tabs.iter().position(|t| t.kind == kind) {
            self.active_tab = Some(i);
        } else {
            self.tabs.push(Tab::new(kind));
            self.active_tab = Some(self.tabs.len() - 1);
        }
        self.overflow_open = false;
        cx.notify();
        self.save_state();
    }

    /// Forward a key event to the appropriate palette TextInput
    /// and run any side-effects (search refresh, result-set
    /// invalidation). Returns `true` if handled.
    pub(crate) fn palette_handle_key(
        &mut self,
        k: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
        let shift = k.modifiers.shift;
        let cmd = k.modifiers.platform || k.modifiers.control;
        let alt = k.modifiers.alt;
        let key_char = k.key_char.as_deref();
        // In Text mode the same input drives bundle search AND
        // the annotation-edit chip (which keeps annotation_edit
        // set while typing); the differentiating logic happens
        // at commit time.
        let target_is_text = self.palette_mode == crate::PaletteMode::Text
            || self.annotation_edit.is_some();
        let app: &mut gpui::App = &mut *cx;
        let handled = if target_is_text {
            self.palette_query
                .handle_key(&k.key, shift, cmd, alt, key_char, app)
        } else {
            self.palette_bin_query
                .handle_key(&k.key, shift, cmd, alt, key_char, app)
        };
        // Side-effects on real text mutations.
        if target_is_text {
            self.palette_selected = 0;
            self.refresh_palette_list();
        } else {
            self.palette_bin_results = None;
            self.palette_bin_error = None;
            self.palette_selected = 0;
            self.refresh_palette_asm_candidates();
        }
        cx.notify();
        handled
    }

    pub(crate) fn palette_move(&mut self, delta: i32, cx: &mut Context<Self>) {
        // In Binary+Asm mode with no results table yet, Up/Down
        // navigates the autocomplete dropdown instead.
        if self.palette_mode == crate::PaletteMode::Binary
            && self.palette_bin_grammar == crate::BinaryGrammar::Asm
            && self.palette_bin_results.is_none()
            && !self.palette_asm_candidates.is_empty()
        {
            self.palette_asm_move(delta, cx);
            return;
        }
        let len = match self.palette_mode {
            crate::PaletteMode::Text => self.palette_list_len,
            crate::PaletteMode::Binary => self
                .palette_bin_results
                .as_ref()
                .map(|r| r.matches.len())
                .unwrap_or(0),
        };
        if len == 0 {
            return;
        }
        let max = len.saturating_sub(1);
        let next = (self.palette_selected as i32 + delta).clamp(0, max as i32) as usize;
        if next != self.palette_selected {
            self.palette_selected = next;
            match self.palette_mode {
                crate::PaletteMode::Text => {
                    self.palette_list_state.scroll_to_reveal_item(next);
                }
                crate::PaletteMode::Binary => {
                    self.palette_bin_list_state.scroll_to_reveal_item(next);
                }
            }
            cx.notify();
        }
    }

    /// Run the binary-search pattern against the currently
    /// selected artifact. Results land in `palette_bin_results`,
    /// errors in `palette_bin_error`. Always called from the
    /// foreground thread; the scan typically runs in milliseconds
    /// for the kind of pattern a user types interactively.
    pub(crate) fn run_palette_bin_search(&mut self, cx: &mut Context<Self>) {
        use glass_arch_arm64::SymbolMap;
        let _ = SymbolMap::default; // touch import for future use
        self.palette_bin_error = None;
        let pattern = self.palette_bin_query.text().to_string();
        let atoms = match self.palette_bin_grammar {
            crate::BinaryGrammar::Bytes => match glass_api::parse_pattern(&pattern) {
                Ok(a) => a,
                Err(e) => {
                    self.palette_bin_error = Some(format!("{e:#}"));
                    cx.notify();
                    return;
                }
            },
            crate::BinaryGrammar::Asm => match glass_api::compile_insn_atoms(&pattern) {
                Ok(atoms) => atoms,
                Err(e) => {
                    self.palette_bin_error = Some(format!("{e:#}"));
                    cx.notify();
                    return;
                }
            },
        };
        let Some(bundle) = self.bundle().cloned() else {
            self.palette_bin_error = Some("no bundle loaded".to_string());
            cx.notify();
            return;
        };
        let Some(artifact) = self.palette_bin_artifact.clone() else {
            self.palette_bin_error = Some("no artifact selected".to_string());
            cx.notify();
            return;
        };
        // Scan every text + data section of the artifact. Mirror
        // the bin-search verb's filter so behaviour matches.
        let mut matches: Vec<glass_api::BinMatch> = Vec::new();
        let mut scanned_sections = 0usize;
        let mut total_bytes_scanned = 0usize;
        // Text sections.
        for ((aid, name), text) in bundle.text_sections.iter() {
            if aid != &artifact {
                continue;
            }
            let bytes: &[u8] = text.bytes.as_ref();
            scanned_sections += 1;
            total_bytes_scanned += bytes.len();
            for (start, slice_end) in glass_api::scan_section(&atoms, bytes) {
                let abs_end = start + slice_end;
                let addr = text.base + start as u64;
                let preview = glass_api::build_preview(
                    true,
                    addr,
                    &bytes[start..abs_end.min(bytes.len())],
                );
                matches.push(glass_api::BinMatch {
                    section: name.clone(),
                    address: format!("0x{addr:x}"),
                    length: slice_end,
                    preview,
                });
            }
        }
        // Data sections (non-text, non-bss, non-debug, non-zero-base).
        // Skipped entirely when `Code only` is checked — the
        // common case where the user is hunting an instruction
        // shape and doesn't want stray ADRP-looking data hits.
        let scan_data = !self.palette_bin_code_only;
        for ((aid, name), data) in bundle.data_sections.iter().filter(|_| scan_data) {
            if aid != &artifact {
                continue;
            }
            if data.base == 0 || data.bytes.is_empty() {
                continue;
            }
            if matches!(data.kind, crate::NativeSectionKind::Bss | crate::NativeSectionKind::Debug) {
                continue;
            }
            let bytes: &[u8] = data.bytes.as_ref();
            scanned_sections += 1;
            total_bytes_scanned += bytes.len();
            for (start, slice_end) in glass_api::scan_section(&atoms, bytes) {
                let abs_end = start + slice_end;
                let addr = data.base + start as u64;
                let preview = glass_api::build_preview(
                    false,
                    addr,
                    &bytes[start..abs_end.min(bytes.len())],
                );
                matches.push(glass_api::BinMatch {
                    section: name.clone(),
                    address: format!("0x{addr:x}"),
                    length: slice_end,
                    preview,
                });
            }
        }
        matches.sort_by(|a, b| a.section.cmp(&b.section).then(a.address.cmp(&b.address)));
        let total = matches.len();
        if scanned_sections == 0 {
            self.palette_bin_error = Some(format!(
                "no sections to scan for artifact {} (bundle has {} text + {} data sections total)",
                artifact.to_string().chars().take(10).collect::<String>(),
                bundle.text_sections.len(),
                bundle.data_sections.len(),
            ));
        } else if total == 0 {
            self.palette_bin_error = Some(format!(
                "no matches across {scanned_sections} sections ({total_bytes_scanned} bytes scanned)"
            ));
        }
        let result = glass_api::BinSearchResult {
            artifact: artifact.to_string(),
            pattern: pattern.clone(),
            total,
            shown: total,
            matches,
        };
        self.palette_bin_list_state =
            ListState::new(total, ListAlignment::Top, px(2000.));
        self.palette_bin_results = Some(std::sync::Arc::new(result));
        self.palette_selected = 0;
        cx.notify();
    }

    /// Navigate to the currently-selected bin-search result.
    /// Same dispatch as a SearchJump: text-section addresses
    /// open the listing, data-section addresses open the hex
    /// view.
    pub(crate) fn palette_bin_activate(&mut self, cx: &mut Context<Self>) {
        let Some(results) = self.palette_bin_results.clone() else { return };
        let Some(m) = results.matches.get(self.palette_selected) else { return };
        let Some(bundle) = self.bundle().cloned() else { return };
        let Some(artifact) = self.palette_bin_artifact.clone() else { return };
        let Ok(addr) = u64::from_str_radix(m.address.trim_start_matches("0x"), 16) else {
            return;
        };
        let section = m.section.clone();
        self.palette_open = false;
        // Text vs data dispatch: ask the bundle which view it is.
        if bundle.text_section_for_addr(&artifact, addr).is_some() {
            self.open_listing_in_new_tab(artifact, section, addr, cx);
        } else {
            self.open_hex_in_new_tab(artifact, section, addr, cx);
        }
    }

    pub(crate) fn open_about(&mut self, cx: &mut Context<Self>) {
        if !self.about_open {
            self.about_open = true;
            cx.notify();
        }
    }

    pub(crate) fn close_about(&mut self, cx: &mut Context<Self>) {
        if self.about_open {
            self.about_open = false;
            cx.notify();
        }
    }

    pub(crate) fn close_annotations_pane(&mut self, cx: &mut Context<Self>) {
        if self.annotations_pane_open {
            self.annotations_pane_open = false;
            self.save_state();
            cx.notify();
        }
    }

    /// Scroll the annotations-pane horizontally by `dx` (positive
    /// = scroll right). Clamps to [0, max_offset].
    pub(crate) fn scroll_annotations_pane_h(
        &mut self,
        dx: Pixels,
        max_offset: Pixels,
        cx: &mut Context<Self>,
    ) {
        let new = (self.annotations_pane_h_offset + dx).clamp(px(0.), max_offset);
        if new != self.annotations_pane_h_offset {
            self.annotations_pane_h_offset = new;
            cx.notify();
        }
    }

    // Used by Phase 4 (edge-icon click + write auto-open). Kept
    // for that wiring even though no current caller exercises it.
    #[allow(dead_code)]
    pub(crate) fn open_annotations_pane(&mut self, cx: &mut Context<Self>) {
        if !self.annotations_pane_open {
            self.annotations_pane_open = true;
            self.save_state();
            cx.notify();
        }
    }

    /// Click handler for an annotations-pane entry. Opens the
    /// appropriate view for the key kind: address → listing, symbol
    /// → resolve through the artifact's symbol map then listing,
    /// class / method → smali tab.
    pub(crate) fn navigate_to_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle().cloned() else { return };
        match key {
            glass_db::AnnotationKey::Address(addr) => {
                if let Some(section) =
                    bundle.text_section_for_addr(&artifact, addr)
                {
                    let section = section.to_string();
                    self.open_listing_at(artifact, section, addr, cx);
                } else if let Some(section) =
                    bundle.data_section_for_addr(&artifact, addr)
                {
                    let section = section.to_string();
                    self.open_hex_in_new_tab(artifact, section, addr, cx);
                }
            }
            glass_db::AnnotationKey::Symbol(name) => {
                let Some(sm) = bundle.symbol_maps.get(&artifact) else { return };
                let Some(sym) = sm.iter().find(|s| {
                    s.display_name == name || s.name == name
                }) else {
                    return;
                };
                let addr = sym.address;
                if let Some(section) =
                    bundle.text_section_for_addr(&artifact, addr)
                {
                    let section = section.to_string();
                    self.open_listing_at(artifact, section, addr, cx);
                }
            }
            glass_db::AnnotationKey::Class(class_jni)
            | glass_db::AnnotationKey::Method(class_jni, _) => {
                let leaf = bundle.resolve(&glass_db::TabState::SmaliClass {
                    class_jni: class_jni.clone(),
                    scroll_line: 0,
                });
                if let Some(leaf) = leaf {
                    self.open_leaf(leaf, cx);
                }
            }
            glass_db::AnnotationKey::MethodLine(class_jni, method_decl, line_offset) => {
                // Look up the `.method` line in the smali body
                // through the pre-built method-line index, then
                // scroll the smali tab to header + line_offset.
                let method_key = format!("{class_jni}->{method_decl}");
                let Some((leaf, header_line)) =
                    bundle.method_lines.get(&method_key).copied()
                else {
                    // Fall back to opening the class — method
                    // index may not have been built (e.g. native).
                    if let Some(leaf) = bundle.resolve(&glass_db::TabState::SmaliClass {
                        class_jni: class_jni.clone(),
                        scroll_line: 0,
                    }) {
                        self.open_leaf(leaf, cx);
                    }
                    return;
                };
                let target_line = header_line + line_offset as usize;
                self.goto_smali_method(leaf, target_line, cx);
            }
        }
    }

    /// Currently-displayed palette entries, taking the scope into
    /// account. When scoped, filter within `scope.entries`; else
    /// query the bundle-wide index. Returned vector is sized to the
    /// palette's display cap.
    pub(crate) fn palette_visible_entries(&self) -> Vec<SearchEntry> {
        const CAP: usize = 50;
        if let Some(scope) = self.palette_scope.as_ref() {
            // Scoped: fixed entry set + fuzzy refinement via the
            // same matching tiers the bundle search uses.
            let q = self.palette_query.text().to_lowercase();
            let mut scored: Vec<(u8, usize, &SearchEntry)> = Vec::new();
            for e in scope.entries.iter() {
                let hay = e.display.to_lowercase();
                let tier = if q.is_empty() {
                    0
                } else if hay.starts_with(&q) {
                    0
                } else if hay.contains(&q) {
                    1
                } else if is_subsequence(&q, &hay) {
                    2
                } else {
                    continue;
                };
                scored.push((tier, e.display.len(), e));
            }
            scored.sort_by_key(|&(tier, len, _)| (tier, len));
            scored
                .into_iter()
                .take(CAP)
                .map(|(_, _, e)| e.clone())
                .collect()
        } else if let Some(idx) = self.search_index.as_ref() {
            idx.filter(self.palette_query.text(), CAP)
                .into_iter()
                .cloned()
                .collect()
        } else {
            Vec::new()
        }
    }

    pub(crate) fn palette_activate(&mut self, cx: &mut Context<Self>) {
        // Annotation edit hijacks Enter: commit the typed value as
        // the new rename / comment.
        if self.annotation_edit.is_some() {
            self.commit_annotation_edit(cx);
            return;
        }
        // Binary mode: Enter on the input row runs the scan;
        // Enter on a result row navigates. The render closure
        // calls palette_bin_activate directly on result clicks,
        // so this branch only handles the input-row case (no
        // results yet) by running the search.
        if self.palette_mode == crate::PaletteMode::Binary {
            if self.palette_bin_results.is_some() {
                self.palette_bin_activate(cx);
            } else {
                self.run_palette_bin_search(cx);
            }
            return;
        }
        let results = self.palette_visible_entries();
        let Some(entry) = results.get(self.palette_selected).cloned() else {
            return;
        };
        let jump = entry.jump.clone();
        self.palette_open = false;
        self.palette_scope = None;
        match jump {
            SearchJump::Listing { artifact, section, addr } => {
                self.open_listing_in_new_tab(artifact, section, addr, cx);
            }
            SearchJump::Hex { artifact, section, addr } => {
                self.open_hex_in_new_tab(artifact, section, addr, cx);
            }
            SearchJump::SmaliClass { class_jni } => {
                // Find the leaf with that class JNI and open it.
                let leaf = self.bundle().and_then(|b| {
                    b.resolve(&glass_db::TabState::SmaliClass {
                        class_jni: class_jni.clone(),
                        scroll_line: 0,
                    })
                });
                if let Some(leaf) = leaf {
                    self.open_leaf(leaf, cx);
                }
            }
            SearchJump::SmaliMethodLine { class_jni, line } => {
                // Open the class then scroll to the absolute line.
                let leaf = self.bundle().and_then(|b| {
                    b.resolve(&glass_db::TabState::SmaliClass {
                        class_jni: class_jni.clone(),
                        scroll_line: 0,
                    })
                });
                if let Some(leaf) = leaf {
                    self.goto_smali_method(leaf, line, cx);
                }
            }
            SearchJump::SectionMap { artifact } => {
                let leaf = self.bundle().and_then(|b| {
                    b.resolve(&glass_db::TabState::SectionMap {
                        artifact: artifact.clone(),
                    })
                });
                if let Some(leaf) = leaf {
                    self.open_leaf(leaf, cx);
                }
            }
        }
        cx.notify();
    }

    /// Recompute `palette_list_len` so up/down navigation knows the
    /// number of currently-displayed rows. Takes the scope into
    /// account.
    pub(crate) fn refresh_palette_list(&mut self) {
        let len = self.palette_visible_entries().len();
        if len != self.palette_list_len {
            self.palette_list_state = ListState::new(len, ListAlignment::Top, px(800.));
            self.palette_list_len = len;
        }
        if self.palette_selected >= len {
            self.palette_selected = 0;
        }
    }

    /// Kick off the background index build on first palette open.
    /// Idempotent — does nothing if already built or in progress.
    pub(crate) fn spawn_search_index_build_if_needed(&mut self, cx: &mut Context<Self>) {
        if self.search_index.is_some() || self.search_indexing {
            return;
        }
        let Some(bundle) = self.bundle().cloned() else { return };
        self.search_indexing = true;
        let task = cx.background_executor().spawn(async move {
            build_search_index(&bundle)
        });
        cx.spawn(async move |this, cx| {
            let idx = task.await;
            let _ = this.update(cx, |shell, cx| {
                shell.search_index = Some(Arc::new(idx));
                shell.search_indexing = false;
                shell.refresh_palette_list();
                cx.notify();
            });
        })
        .detach();
    }

    /// Move the active listing tab's selection by `delta`
    /// (typically -1 / +1 from Up/Down). Clamps to the valid
    /// row range. Driven by the global Up/Down action handlers
    /// when no edit / palette / dialog is active.
    /// Move the selected byte one position left/right on the
    /// active hex tab. Wraps across row boundaries — going left
    /// off the start of a row lands on the last byte of the
    /// previous row; going right off the end lands on the
    /// first byte of the next row.
    pub(crate) fn hex_move_byte(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        if !matches!(tab.kind, crate::TabKind::Hex { .. }) {
            return;
        }
        let Some(rows) = tab.hex_rows.as_ref() else { return };
        // Find the (row_index, byte_addr) of the currently
        // selected byte. If there's no selection, start at
        // the first byte of the first Bytes row.
        let (mut row_idx, mut addr) = match tab.selected_byte_addr {
            Some(a) => {
                let row = rows.iter().position(|r| match r {
                    crate::HexRow::Bytes { address, .. }
                        if a >= *address && a < *address + 16 =>
                    {
                        true
                    }
                    _ => false,
                });
                match row {
                    Some(i) => (i, a),
                    None => return,
                }
            }
            None => {
                let row = rows.iter().enumerate().find_map(|(i, r)| {
                    matches!(r, crate::HexRow::Bytes { .. }).then_some(i)
                });
                let Some(i) = row else { return };
                let crate::HexRow::Bytes { address, .. } = &rows[i] else {
                    return;
                };
                (i, *address)
            }
        };
        let step: i64 = delta.signum() as i64;
        let new_addr = (addr as i64 + step) as u64;
        // Stay inside the current row if we can.
        if let crate::HexRow::Bytes { address, .. } = &rows[row_idx] {
            if new_addr >= *address && new_addr < *address + 16 {
                addr = new_addr;
            } else {
                // Cross to the prev/next Bytes row.
                let next_row_idx = if step > 0 {
                    rows.iter()
                        .enumerate()
                        .skip(row_idx + 1)
                        .find_map(|(i, r)| matches!(r, crate::HexRow::Bytes { .. }).then_some(i))
                } else if row_idx == 0 {
                    None
                } else {
                    rows.iter()
                        .enumerate()
                        .take(row_idx)
                        .rev()
                        .find_map(|(i, r)| matches!(r, crate::HexRow::Bytes { .. }).then_some(i))
                };
                let Some(ni) = next_row_idx else { return };
                let crate::HexRow::Bytes { address, .. } = &rows[ni] else {
                    return;
                };
                row_idx = ni;
                addr = if step > 0 { *address } else { *address + 15 };
            }
        }
        tab.selected_row = Some(row_idx);
        tab.selected_byte_addr = Some(addr);
        tab.scroll.scroll_to_reveal_item(row_idx);
        cx.notify();
    }

    /// Enter on a hex tab: open the edit at the selected byte
    /// (string popover if it's inside a recognised string item,
    /// single-byte edit otherwise). Returns true if it acted on
    /// the active tab — caller can chain further fallbacks if
    /// false.
    pub(crate) fn hex_open_edit_at_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(active) = self.active_tab else { return false };
        let Some(tab) = self.tabs.get(active) else { return false };
        let artifact = match &tab.kind {
            crate::TabKind::Hex { artifact, .. } => artifact.clone(),
            _ => return false,
        };
        let Some(addr) = tab.selected_byte_addr else { return false };
        // Prefer string edit when the click lands inside a
        // recognised string item, same heuristic the
        // double-click handler uses.
        let bundle = match self.bundle() {
            Some(b) => b.clone(),
            None => return false,
        };
        let in_string = bundle
            .data_section_for_addr(&artifact, addr)
            .map(|name| {
                name.contains("cstring")
                    || name.contains("__cfstring")
                    || name.contains("__objc_methname")
            })
            .unwrap_or(false)
            && crate::listing_render::item_extent_for(&bundle, &artifact, addr).is_some();
        if in_string {
            self.begin_hex_string_edit(artifact, addr, cx);
        } else {
            self.begin_hex_byte_edit(artifact, addr, cx);
        }
        true
    }

    /// Animated scroll by ~one viewport in the active tab.
    /// Works on any tab kind that uses the standard `ListState`
    /// (listing, hex, smali). Selection cursor stays in place.
    /// Tick at 60 fps over ~150 ms (≈9 frames).
    pub(crate) fn listing_page_scroll(&mut self, direction: i32, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get(active) else { return };
        let viewport_h = tab.scroll.viewport_bounds().size.height;
        if viewport_h <= gpui::px(0.) {
            return;
        }
        // 90% of a viewport so the row at the boundary stays
        // visible — same rule most editors use.
        let total_px = viewport_h * 0.9;
        const FRAMES: usize = 9;
        let per_frame = if direction > 0 {
            total_px / FRAMES as f32
        } else {
            -total_px / FRAMES as f32
        };
        let scroll = tab.scroll.clone();
        cx.spawn(async move |this, cx| {
            for _ in 0..FRAMES {
                scroll.scroll_by(per_frame);
                let _ = this.update(cx, |_s, cx| cx.notify());
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
            }
        })
        .detach();
    }

    /// Move the row selection on whichever tab kind is active.
    /// Listing tabs skip past non-Instruction rows (separators,
    /// symbol headers); hex / smali tabs just clamp to row count.
    pub(crate) fn move_listing_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        let step: i32 = if delta >= 0 { 1 } else { -1 };
        // Dispatch by tab kind: each one owns a different row
        // collection.
        let (row_count, only_instructions) = match &tab.kind {
            crate::TabKind::Listing { .. } => match tab.listing_rows.as_ref() {
                Some(rows) => (rows.len(), true),
                None => return,
            },
            crate::TabKind::Hex { .. } => match tab.hex_rows.as_ref() {
                Some(rows) => (rows.len(), false),
                None => return,
            },
            crate::TabKind::SmaliClass { .. } => match tab.lines.as_ref() {
                Some(lines) => (lines.len(), false),
                None => return,
            },
            _ => return,
        };
        if row_count == 0 {
            return;
        }
        let max = row_count as i32 - 1;
        let mut pos = tab.selected_row.unwrap_or(0) as i32;
        if only_instructions {
            // Listing: walk past non-Instruction rows.
            let rows = tab.listing_rows.as_ref().unwrap().clone();
            loop {
                let next = pos + step;
                if next < 0 || next > max {
                    return;
                }
                pos = next;
                if matches!(rows[pos as usize], crate::ListingRow::Instruction { .. }) {
                    break;
                }
            }
        } else {
            let next = (pos + step).clamp(0, max);
            if next == pos {
                return;
            }
            pos = next;
        }
        let next = pos as usize;
        if tab.selected_row != Some(next) {
            tab.selected_row = Some(next);
            // Hex tabs also drive `selected_byte_addr` so the
            // byte cursor moves with the row.
            if matches!(tab.kind, crate::TabKind::Hex { .. }) {
                if let Some(rows) = tab.hex_rows.as_ref() {
                    if let Some(crate::HexRow::Bytes { address, .. }) = rows.get(next) {
                        // Preserve the column offset within the
                        // row so vertical movement keeps the
                        // byte cursor under the same column.
                        let column = tab
                            .selected_byte_addr
                            .and_then(|a| {
                                rows.iter().find_map(|r| match r {
                                    crate::HexRow::Bytes { address, .. }
                                        if a >= *address && a < *address + 16 =>
                                    {
                                        Some(a - *address)
                                    }
                                    _ => None,
                                })
                            })
                            .unwrap_or(0);
                        tab.selected_byte_addr = Some(*address + column);
                    }
                }
            }
            tab.scroll.scroll_to_reveal_item(next);
            cx.notify();
        }
    }

    /// If the active listing tab has a selected instruction row,
    /// open it for editing. Bound to Enter when no other context
    /// (palette / dialog / in-flight edit) is consuming Enter.
    pub(crate) fn edit_selected_listing_row(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get(active) else { return };
        let Some(selected) = tab.selected_row else { return };
        let Some(rows) = tab.listing_rows.as_ref() else { return };
        let Some(row) = rows.get(selected) else { return };
        let crate::ListingRow::Instruction { address, .. } = row else {
            return;
        };
        let address = *address;
        let artifact = match &tab.kind {
            crate::TabKind::Listing { artifact, .. } => artifact.clone(),
            _ => return,
        };
        self.begin_disasm_edit_at_address(artifact, address, cx);
    }

    pub(crate) fn select_active_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        let mut changed = false;
        if tab.selected_row != Some(row) {
            tab.selected_row = Some(row);
            changed = true;
        }
        if tab.selected_byte_addr.is_some() {
            tab.selected_byte_addr = None;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Hex-view: set the highlighted byte on the active tab. Caller is
    /// responsible for having set the matching row via
    /// `select_active_row` first.
    pub(crate) fn select_byte(&mut self, addr: u64, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        if tab.selected_byte_addr != Some(addr) {
            tab.selected_byte_addr = Some(addr);
            cx.notify();
        }
    }

    /// Add `dx` (positive scrolls right) to the active tab's horizontal
    /// offset, clamped to [0, max].
    pub(crate) fn scroll_h_by(&mut self, dx: Pixels, max: Pixels, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        let new_offset = (tab.h_offset + dx).clamp(px(0.), max);
        if (new_offset - tab.h_offset).abs() > px(0.1) {
            tab.h_offset = new_offset;
            cx.notify();
        }
    }

    /// Address-click inside a Listing tab: reuse the active tab (or
    /// match by kind if the active tab isn't a Listing), scroll to addr.
    /// Use `open_listing_in_new_tab` from tree / SectionMap clicks where
    /// the user expects a fresh tab.
    /// Open (or focus) the SmaliClass tab for `target_leaf` and scroll
    /// it so `line_no` is the selected, near-top row.
    pub(crate) fn goto_smali_method(
        &mut self,
        target_leaf: LeafId,
        line_no: usize,
        cx: &mut Context<Self>,
    ) {
        // Reuse the existing open_leaf path so we get tab dedupe + the
        // line-cache rebuild on first activation.
        self.open_leaf(target_leaf, cx);
        // Find the active tab (= the smali tab we just opened), set
        // the row + scroll. ensure_active_tab_lines runs on the next
        // paint via render, which builds tab.lines and sizes tab.scroll
        // — *after* that, scroll-to becomes meaningful. We schedule the
        // scroll for the next frame via a tiny defer.
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.selected_row = Some(line_no);
                tab.pending_smali_scroll_line = Some(line_no);
            }
        }
        cx.notify();
        self.save_state();
    }

    /// Find the text section containing `addr` and open / focus a
    /// Listing tab scrolled to it. Used by CFG-block clicks where we
    /// only know the address, not the section name.
    pub(crate) fn open_listing_at_addr(
        &mut self,
        artifact: glass_db::ArtifactId,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        let section = match self
            .bundle()
            .and_then(|b| b.text_section_for_addr(&artifact, addr))
        {
            Some(s) => s.to_string(),
            None => return,
        };
        self.open_listing_at(artifact, section, addr, cx);
    }

    pub(crate) fn open_listing_at(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        self.overflow_open = false;
        let kind = TabKind::Listing {
            artifact: artifact.clone(),
            section: section.clone(),
        };
        // Prefer reusing the active tab when it's already a Listing for
        // this same section — that's the click-an-operand path.
        let active_matches = self
            .active_tab
            .and_then(|i| self.tabs.get(i))
            .map(|t| t.kind == kind)
            .unwrap_or(false);
        let idx = if active_matches {
            self.active_tab.unwrap()
        } else {
            // Otherwise pick any matching tab, else open a new one.
            match self.tabs.iter().position(|t| t.kind == kind) {
                Some(i) => i,
                None => {
                    self.tabs.push(Tab::new(kind));
                    self.tabs.len() - 1
                }
            }
        };
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.pending_scroll_addr = Some(addr);
        }
        self.active_tab = Some(idx);
        cx.notify();
        self.save_state();
    }

    /// Reuse an existing Hex tab for `(artifact, section)` if one is
    /// open, else push a new one. Scrolls to `addr`. Use the
    /// `open_hex_force_new_tab` variant for explicit "Follow in new
    /// tab" gestures.
    pub(crate) fn open_hex_in_new_tab(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        self.overflow_open = false;
        let kind = TabKind::Hex { artifact, section };
        let idx = match self.tabs.iter().position(|t| t.kind == kind) {
            Some(i) => i,
            None => {
                self.tabs.push(Tab::new(kind));
                self.tabs.len() - 1
            }
        };
        self.tabs[idx].pending_scroll_addr = Some(addr);
        self.active_tab = Some(idx);
        cx.notify();
        self.save_state();
    }

    /// Always open a fresh Hex tab — no dedupe. Used by the "Follow
    /// in new tab" / shift-click flow.
    pub(crate) fn open_hex_force_new_tab(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        self.overflow_open = false;
        let kind = TabKind::Hex { artifact, section };
        self.tabs.push(Tab::new(kind));
        let idx = self.tabs.len() - 1;
        self.tabs[idx].pending_scroll_addr = Some(addr);
        self.active_tab = Some(idx);
        cx.notify();
        self.save_state();
    }

    pub(crate) fn open_listing_in_new_tab(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        self.overflow_open = false;
        let kind = TabKind::Listing { artifact, section };
        // Dedupe per-section. Listing tabs are now id-tracked rather
        // than kind-tracked, so duplicates *are* safe — but the
        // common "open another listing" gesture (tree clicks,
        // overview clicks) means "show me that section", and
        // reusing an existing tab is the right UX. Use
        // `open_listing_force_new_tab` for explicit "new tab"
        // gestures (shift-click, context menu).
        let idx = match self.tabs.iter().position(|t| t.kind == kind) {
            Some(i) => i,
            None => {
                self.tabs.push(Tab::new(kind));
                self.tabs.len() - 1
            }
        };
        self.tabs[idx].pending_scroll_addr = Some(addr);
        self.active_tab = Some(idx);
        cx.notify();
        self.save_state();
    }

    /// Always open a fresh Listing tab — no dedupe. Used by the
    /// "Follow in new tab" / shift-click flow.
    pub(crate) fn open_listing_force_new_tab(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        self.overflow_open = false;
        let kind = TabKind::Listing { artifact, section };
        self.tabs.push(Tab::new(kind));
        let idx = self.tabs.len() - 1;
        self.tabs[idx].pending_scroll_addr = Some(addr);
        self.active_tab = Some(idx);
        cx.notify();
        self.save_state();
    }

    /// Open the tab corresponding to a tree leaf. SmaliClass + SectionMap
    /// dedupe by kind (one tab per class / one map per artifact makes
    /// sense). Listing always opens fresh — see `open_listing_in_new_tab`.
    pub(crate) fn open_leaf(&mut self, leaf: LeafId, cx: &mut Context<Self>) {
        self.overflow_open = false;
        let kind = {
            let Some(bundle) = self.bundle() else { return };
            let Some(kind_src) = bundle.kinds.get(leaf.0) else { return };
            TabKind::from_kind(kind_src)
        };
        // Listing leaves want a fresh tab on every click.
        if let TabKind::Listing { artifact, section } = &kind {
            let artifact = artifact.clone();
            let section = section.clone();
            // Open scrolled to the section base — no specific address.
            let base = self
                .bundle()
                .and_then(|b| b.text_sections.get(&(artifact.clone(), section.clone())))
                .map(|t| t.base)
                .unwrap_or(0);
            self.open_listing_in_new_tab(artifact, section, base, cx);
            return;
        }
        match self.tabs.iter().position(|t| t.kind == kind) {
            Some(i) => {
                if self.active_tab != Some(i) {
                    self.active_tab = Some(i);
                    cx.notify();
                    self.save_state();
                }
            }
            None => {
                self.tabs.push(Tab::new(kind));
                self.active_tab = Some(self.tabs.len() - 1);
                cx.notify();
                self.save_state();
            }
        }
    }

    pub(crate) fn focus_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.overflow_open = false;
        if index < self.tabs.len() && self.active_tab != Some(index) {
            self.active_tab = Some(index);
            cx.notify();
            self.save_state();
        }
    }

    pub(crate) fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        self.active_tab = if self.tabs.is_empty() {
            None
        } else {
            // Prefer the tab now at `index` (the one that took its place);
            // if we closed the last tab, fall back to the new last.
            Some(index.min(self.tabs.len() - 1))
        };
        // Keep dropdown open only if there are still hidden tabs to show.
        cx.notify();
        self.save_state();
    }

    // ---- Annotation write helpers (Phase 4) -----------------------

    /// Merge-mutate an annotation slot. Read whatever's currently
    /// stored, apply `mutate`, persist via `set_annotation` (or
    /// `clear_annotation` if the result has every facet unset),
    /// then refresh the in-memory index on `LoadedBundle` so the
    /// renderer + pane pick up the change immediately. Auto-opens
    /// the annotations pane on a successful first-write.
    pub(crate) fn write_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        mutate: impl FnOnce(&mut glass_db::Annotation),
    ) -> anyhow::Result<()> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no glass-db handle"))?;
        let mut current = db
            .load_annotations(&artifact)?
            .into_iter()
            .find(|(k, _)| k == &key)
            .map(|(_, v)| v)
            .unwrap_or_default();
        mutate(&mut current);
        if current.is_empty() {
            db.clear_annotation(artifact.clone(), key.clone());
        } else {
            db.set_annotation(artifact.clone(), key.clone(), current.clone());
        }
        db.flush()?;
        self.refresh_artifact_annotations(&artifact)?;
        if !self.annotations_pane_open {
            self.annotations_pane_open = true;
            self.save_state();
        }
        Ok(())
    }

    /// Remove every facet at a key. Used by "Clear annotation".
    pub(crate) fn clear_annotation_full(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
    ) -> anyhow::Result<()> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no glass-db handle"))?;
        db.clear_annotation(artifact.clone(), key);
        db.flush()?;
        self.refresh_artifact_annotations(&artifact)?;
        Ok(())
    }

    /// Re-read annotations for every artifact in the loaded
    /// bundle. Called by the DB-mtime poller so writes made via
    /// CLI / MCP land in the GUI without a bundle reload.
    pub(crate) fn refresh_all_annotations(&mut self, cx: &mut Context<Self>) {
        let Some(ids) = self
            .bundle()
            .map(|b| b.artifact_ids.as_ref().clone())
        else {
            return;
        };
        for aid in ids.iter() {
            if let Err(e) = self.refresh_artifact_annotations(aid) {
                tracing::warn!(artifact = %aid, "annotation reload failed: {e:#}");
            }
        }
        cx.notify();
    }

    /// Rebuild the per-artifact AnnotationIndex from the DB and
    /// splice it back into the LoadedBundle. The bundle's
    /// `annotations` map is Arc-wrapped, so we make_mut the outer
    /// and only the one artifact's entry is rebuilt.
    fn refresh_artifact_annotations(
        &mut self,
        artifact: &glass_db::ArtifactId,
    ) -> anyhow::Result<()> {
        // Compute the new index while only the DB is borrowed,
        // then drop that borrow before mutably grabbing the bundle.
        let new_index = match self.db.as_ref() {
            Some(db) => crate::annotations::load_for_artifacts(
                db,
                std::slice::from_ref(artifact),
            ),
            None => return Ok(()),
        };
        let Some(bundle) = self.bundle_mut() else { return Ok(()) };
        let map = std::sync::Arc::make_mut(&mut bundle.annotations);
        if let Some((aid, idx)) = new_index.into_iter().next() {
            map.insert(aid, idx);
        } else {
            map.remove(artifact);
        }
        Ok(())
    }

    // ---- Disasm edit -------------------------------------------------

    /// Convenience: look up the disasm at `(artifact, address)`
    /// in the bundle and open the edit field pre-populated with
    /// that text. Called by the listing row's double-click handler.
    pub(crate) fn begin_disasm_edit_at_address(
        &mut self,
        artifact: glass_db::ArtifactId,
        address: u64,
        cx: &mut Context<Self>,
    ) {
        let initial = match self
            .bundle()
            .and_then(|b| b.bytes_at(&artifact, address))
        {
            Some(bytes) => decode_insn_pretty(&bytes, address),
            None => return,
        };
        self.begin_disasm_edit(artifact, address, initial, cx);
    }

    /// Enter edit mode for the disasm row at `(artifact, address)`.
    /// Pre-populates the input with the original mnemonic + operands.
    pub(crate) fn begin_disasm_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        address: u64,
        initial_text: String,
        cx: &mut Context<Self>,
    ) {
        let mut input = crate::text_input::TextInput::from_text(initial_text);
        // Select the whole pre-populated text so the first
        // keystroke replaces it — this is the common case for
        // editing a disasm row (overtype rather than amend).
        input.select_all_pub();
        self.disasm_edit = Some(crate::DisasmEditState {
            artifact,
            address,
            input,
            error: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
        });
        self.refresh_disasm_edit_suggestions();
        cx.notify();
    }

    /// Forward a key event into the active disasm edit's TextInput.
    /// Returns `true` if the edit consumed the event (any printable
    /// key, arrow keys, etc.). Returns `false` for unhandled keys so
    /// the caller can decide what to do.
    pub(crate) fn disasm_edit_handle_key(
        &mut self,
        k: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
        // Up/Down navigate the suggestion list when one is showing.
        // Tab commits the highlighted suggestion. Other keys
        // forward to the TextInput.
        if k.key == "up" {
            self.move_disasm_suggestion(-1, cx);
            return true;
        }
        if k.key == "down" {
            self.move_disasm_suggestion(1, cx);
            return true;
        }
        if k.key == "tab" {
            self.commit_disasm_suggestion(cx);
            return true;
        }
        let Some(edit) = self.disasm_edit.as_mut() else {
            return false;
        };
        let shift = k.modifiers.shift;
        let cmd = k.modifiers.platform || k.modifiers.control;
        let alt = k.modifiers.alt;
        let key_char = k.key_char.as_deref();
        let app: &mut gpui::App = &mut *cx;
        let _changed = edit
            .input
            .handle_key(&k.key, shift, cmd, alt, key_char, app);
        edit.error = None;
        self.refresh_disasm_edit_suggestions();
        cx.notify();
        true
    }

    /// Mouse-click handler for a suggestion row. Highlights the
    /// clicked index and commits it. Used by the suggestion
    /// overlay's per-row click handlers.
    pub(crate) fn click_disasm_suggestion(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(e) = self.disasm_edit.as_mut() {
            if index < e.suggestions.len() {
                e.suggestion_selected = index;
            } else {
                return;
            }
        } else {
            return;
        }
        self.commit_disasm_suggestion(cx);
    }

    pub(crate) fn move_disasm_suggestion_pub(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.move_disasm_suggestion(delta, cx);
    }

    fn move_disasm_suggestion(&mut self, delta: i32, cx: &mut Context<Self>) {
        if let Some(e) = self.disasm_edit.as_mut() {
            if e.suggestions.is_empty() {
                return;
            }
            let max = e.suggestions.len() - 1;
            let next = (e.suggestion_selected as i32 + delta).clamp(0, max as i32) as usize;
            if next != e.suggestion_selected {
                e.suggestion_selected = next;
                cx.notify();
            }
        }
    }

    pub(crate) fn commit_disasm_suggestion_pub(&mut self, cx: &mut Context<Self>) {
        self.commit_disasm_suggestion(cx);
    }

    /// Replace the partial word at the cursor with the
    /// highlighted suggestion's `commit_text`. Cursor lands at
    /// the end of the inserted text. The dropdown is dismissed
    /// until the next keystroke so the user gets a clear path
    /// to "Enter commits the whole edit" — if we re-classified
    /// immediately, suggestions for the *next* slot would pop
    /// up and Enter would never reach `commit_disasm_edit`.
    fn commit_disasm_suggestion(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.disasm_edit.as_mut() else { return };
        let Some(sugg) = edit.suggestions.get(edit.suggestion_selected).cloned()
        else {
            return;
        };
        let text = edit.input.text().to_string();
        let cursor = edit.input.cursor();
        let ctx = glass_api::classify_insn_cursor(&text, cursor);
        let mut new_text = String::with_capacity(text.len() + sugg.commit_text.len());
        new_text.push_str(&text[..ctx.word_range.start]);
        new_text.push_str(&sugg.commit_text);
        new_text.push_str(&text[ctx.word_range.end..]);
        edit.input.set_text(new_text);
        edit.suggestions.clear();
        edit.suggestion_selected = 0;
        cx.notify();
    }

    /// Rebuild the suggestion list based on the current input +
    /// cursor. Cheap — symbol scans are linear over the
    /// artifact's symbol map (typically a few thousand entries).
    pub(crate) fn refresh_disasm_edit_suggestions(&mut self) {
        // Pull input + artifact before grabbing the mutable
        // borrow so we can still read `self.bundle()` while
        // building the suggestion list.
        let (text, artifact) = match self.disasm_edit.as_ref() {
            Some(e) => (e.input.text().to_string(), e.artifact.clone()),
            None => return,
        };
        let cursor = self
            .disasm_edit
            .as_ref()
            .map(|e| e.input.cursor())
            .unwrap_or(text.len());
        let ctx = glass_api::classify_insn_cursor(&text, cursor);
        let mut out: Vec<crate::EditSuggestion> = Vec::new();
        match ctx.kind {
            glass_api::CursorKind::Mnemonic => {
                let variants = glass_api::match_insn_variants(&ctx.partial, 12);
                for cand in variants {
                    out.push(crate::EditSuggestion {
                        label: cand.variant.template.clone().into(),
                        commit_text: cand.variant.mnemonic.to_string(),
                        detail: "asm".into(),
                        kind: crate::EditSuggestionKind::Mnemonic,
                    });
                }
            }
            glass_api::CursorKind::BranchTargetSlot => {
                if let Some(bundle) = self.bundle() {
                    if let Some(map) = bundle.symbol_maps.get(&artifact) {
                        let needle = ctx.partial.as_str();
                        let mut hits: Vec<(usize, _)> = Vec::new();
                        for sym in map.iter() {
                            let display_lower = sym.display_name.to_ascii_lowercase();
                            let raw_lower = sym.name.to_ascii_lowercase();
                            let score = if display_lower.starts_with(needle) {
                                0
                            } else if raw_lower.starts_with(needle) {
                                1
                            } else if display_lower.contains(needle) {
                                2
                            } else if raw_lower.contains(needle) {
                                3
                            } else {
                                continue;
                            };
                            hits.push((score, sym));
                        }
                        hits.sort_by_key(|(s, _)| *s);
                        hits.truncate(20);
                        for (_, sym) in hits {
                            out.push(crate::EditSuggestion {
                                label: sym.display_name.clone().into(),
                                commit_text: sym.display_name.clone(),
                                detail: format!("0x{:x}", sym.address).into(),
                                kind: crate::EditSuggestionKind::Symbol,
                            });
                        }
                    }
                }
            }
            glass_api::CursorKind::RegisterSlot => {
                let needle = ctx.partial.as_str();
                for &name in REGISTER_NAMES {
                    if name.starts_with(needle) {
                        out.push(crate::EditSuggestion {
                            label: name.into(),
                            commit_text: name.to_string(),
                            detail: "reg".into(),
                            kind: crate::EditSuggestionKind::Register,
                        });
                        if out.len() >= 20 {
                            break;
                        }
                    }
                }
            }
            glass_api::CursorKind::ImmediateSlot | glass_api::CursorKind::MemorySlot => {
                // No suggestions; raw input.
            }
        }
        if let Some(edit) = self.disasm_edit.as_mut() {
            edit.suggestions = out;
            edit.suggestion_selected = 0;
        }
    }

    /// Try to compile + stage the in-progress edit. On success the
    /// edit is added to the bundle's `edits` registry; on failure
    /// the error chip is updated and edit mode stays open.
    pub(crate) fn commit_disasm_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.disasm_edit.clone() else { return };
        let source_text = edit.input.text().to_string();
        // Look up the original bytes from the bundle so we can
        // store them for revert / dialog rendering.
        let Some(bundle) = self.bundle() else { return };
        let original = bundle.bytes_at(&edit.artifact, edit.address);
        let Some(original_bytes) = original else {
            if let Some(e) = self.disasm_edit.as_mut() {
                e.error = Some(format!("no instruction at 0x{:x}", edit.address));
            }
            cx.notify();
            return;
        };
        // Compile with the row's address (so PC-relative
        // encodings come out correctly) and a symbol resolver
        // backed by the artifact's symbol map (so `bl foo`
        // works).
        let sym_map = bundle.symbol_maps.get(&edit.artifact).cloned();
        let lookup: Box<dyn Fn(&str) -> Option<u64>> = match sym_map {
            Some(map) => Box::new(move |needle: &str| {
                map.iter()
                    .find(|s| s.display_name == needle || s.name == needle)
                    .map(|s| s.address)
            }),
            None => Box::new(|_| None),
        };
        let new_bytes = match glass_api::compile_insn_at(
            &source_text,
            edit.address,
            Some(lookup.as_ref()),
        ) {
            Ok(b) if b.len() == 4 => [b[0], b[1], b[2], b[3]],
            Ok(_) => {
                if let Some(e) = self.disasm_edit.as_mut() {
                    e.error = Some("must encode exactly one instruction".to_string());
                }
                cx.notify();
                return;
            }
            Err(err) => {
                if let Some(e) = self.disasm_edit.as_mut() {
                    e.error = Some(format!("{err:#}"));
                }
                cx.notify();
                return;
            }
        };
        // Decode the new bytes for the cached pretty-print,
        // resolving address operands to symbol names so the
        // modified row + Changes dialog show `bl decode_packet`
        // instead of `bl 0x...`.
        let sym_map_for_display = bundle.symbol_maps.get(&edit.artifact).cloned();
        let new_disasm = decode_insn_pretty_with_symbols(
            &new_bytes,
            edit.address,
            |addr: u64| {
                sym_map_for_display
                    .as_ref()
                    .and_then(|m| m.at(addr).map(|s| s.display_name.clone()))
            },
        );
        let staged = crate::edits::Edit {
            artifact: edit.artifact.clone(),
            vaddr: edit.address,
            kind: crate::edits::EditKind::Instruction,
            new_bytes: new_bytes.to_vec(),
            original_bytes: original_bytes.to_vec(),
            source_text,
            display: new_disasm,
        };
        if let Some(b) = self.bundle_mut() {
            b.edits.insert(staged);
        }
        self.disasm_edit = None;
        cx.notify();
    }

    pub(crate) fn cancel_disasm_edit(&mut self, cx: &mut Context<Self>) {
        if self.disasm_edit.take().is_some() {
            cx.notify();
        }
    }

    // ---- Hex edit -----------------------------------------------------

    /// Open a string-item edit at `addr`. Uses the existing
    /// `item_extent_for` heuristic to determine the item bounds
    /// (covering symbol size, or NUL-scan in strings sections),
    /// reads up to the first NUL within that range as the
    /// initial text, then displays a popover.
    pub(crate) fn begin_hex_string_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle() else { return };
        let Some((start, end)) = crate::listing_render::item_extent_for(
            bundle, &artifact, addr,
        ) else {
            return;
        };
        let length = end.saturating_sub(start) as usize;
        if length == 0 {
            return;
        }
        // Decode bytes [start, end) up to first NUL as the
        // editable text. Reuses data_byte_at so already-staged
        // edits show their live content in the popover.
        let mut decoded = String::new();
        for off in 0..length {
            let Some(b) = bundle.data_byte_at(&artifact, start + off as u64)
            else { break };
            if b == 0 {
                break;
            }
            if (0x20..=0x7e).contains(&b) {
                decoded.push(b as char);
            } else {
                decoded.push('·');
            }
        }
        self.hex_edit = Some(crate::HexEditState {
            artifact,
            address: start,
            length,
            input: crate::text_input::TextInput::from_text(decoded),
            error: None,
            kind: crate::HexEditKind::String,
        });
        cx.notify();
    }

    /// Open a single-byte edit at `addr`. Pre-populates the
    /// input with the current byte's hex pair (which may already
    /// be a staged edit — `bundle.data_byte_at` is the source of
    /// truth, not the underlying file bytes).
    pub(crate) fn begin_hex_byte_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle() else { return };
        let Some(b) = bundle.data_byte_at(&artifact, addr) else { return };
        self.hex_edit = Some(crate::HexEditState {
            artifact,
            address: addr,
            length: 1,
            input: crate::text_input::TextInput::from_text(format!("{b:02x}")),
            error: None,
            kind: crate::HexEditKind::Byte,
        });
        cx.notify();
    }

    pub(crate) fn hex_edit_handle_key(
        &mut self,
        k: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(edit) = self.hex_edit.as_mut() else { return false };
        let shift = k.modifiers.shift;
        let cmd = k.modifiers.platform || k.modifiers.control;
        let alt = k.modifiers.alt;
        let key_char = k.key_char.as_deref();
        let app: &mut gpui::App = &mut *cx;
        let _ = edit
            .input
            .handle_key(&k.key, shift, cmd, alt, key_char, app);
        edit.error = None;
        cx.notify();
        true
    }

    /// Commit the in-flight hex edit. Validates the input
    /// against `kind` (1 hex pair for Byte, length-bounded
    /// string for String) and stages an Edit. Closes the edit
    /// on success; leaves it open with an error chip on failure.
    pub(crate) fn commit_hex_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.hex_edit.clone() else { return };
        let source = edit.input.text().trim().to_string();
        let Some(bundle) = self.bundle() else { return };
        // Look up the original bytes for this span.
        let mut original = Vec::with_capacity(edit.length);
        for off in 0..edit.length {
            let Some(b) = bundle.data_byte_at(&edit.artifact, edit.address + off as u64)
            else {
                if let Some(e) = self.hex_edit.as_mut() {
                    e.error = Some("address span runs past section end".to_string());
                }
                cx.notify();
                return;
            };
            original.push(b);
        }
        let new_bytes_result: Result<Vec<u8>, String> = match edit.kind {
            crate::HexEditKind::Byte => {
                let s = source.replace(' ', "");
                if s.len() != 2 {
                    Err("expected 2 hex digits".to_string())
                } else if let Ok(b) = u8::from_str_radix(&s, 16) {
                    Ok(vec![b])
                } else {
                    Err(format!("not hex: {s:?}"))
                }
            }
            crate::HexEditKind::String => {
                // The new bytes occupy exactly the original
                // item length (`edit.length`). Anything shorter
                // gets NUL-padded — and since the buffer is
                // zero-initialised, that's automatic. We only
                // reject inputs that would fill the whole span
                // with no room for a NUL terminator, which
                // would break readers scanning for one.
                let raw = source.as_bytes();
                if raw.len() > edit.length {
                    Err(format!(
                        "string is {} bytes; max {}",
                        raw.len(),
                        edit.length
                    ))
                } else if raw.len() == edit.length && !raw.contains(&0) {
                    Err(format!(
                        "string is {} bytes — no room for the trailing NUL; \
                         shorten by 1 byte",
                        raw.len()
                    ))
                } else {
                    let mut v = vec![0u8; edit.length];
                    v[..raw.len()].copy_from_slice(raw);
                    Ok(v)
                }
            }
        };
        let new_bytes = match new_bytes_result {
            Ok(v) => v,
            Err(msg) => {
                if let Some(e) = self.hex_edit.as_mut() {
                    e.error = Some(msg);
                }
                cx.notify();
                return;
            }
        };
        let (kind_label, display) = match edit.kind {
            crate::HexEditKind::Byte => (
                crate::edits::EditKind::Bytes,
                format!("{:02x}", new_bytes[0]),
            ),
            crate::HexEditKind::String => {
                (crate::edits::EditKind::String, source.clone())
            }
        };
        let staged = crate::edits::Edit {
            artifact: edit.artifact.clone(),
            vaddr: edit.address,
            kind: kind_label,
            new_bytes,
            original_bytes: original,
            source_text: source,
            display,
        };
        if let Some(b) = self.bundle_mut() {
            b.edits.insert(staged);
        }
        self.hex_edit = None;
        cx.notify();
    }

    pub(crate) fn cancel_hex_edit(&mut self, cx: &mut Context<Self>) {
        if self.hex_edit.take().is_some() {
            cx.notify();
        }
    }

    // ---- Changes dialog ----------------------------------------------

    pub(crate) fn toggle_changes_dialog(&mut self, cx: &mut Context<Self>) {
        self.changes_dialog_open = !self.changes_dialog_open;
        self.changes_dialog_confirm_abandon = false;
        cx.notify();
    }

    pub(crate) fn open_changes_dialog(&mut self, cx: &mut Context<Self>) {
        self.changes_dialog_open = true;
        self.changes_dialog_confirm_abandon = false;
        cx.notify();
    }

    pub(crate) fn close_changes_dialog(&mut self, cx: &mut Context<Self>) {
        if self.changes_dialog_open {
            self.changes_dialog_open = false;
            self.changes_dialog_confirm_abandon = false;
            cx.notify();
        }
    }

    pub(crate) fn arm_abandon_confirm(&mut self, cx: &mut Context<Self>) {
        self.changes_dialog_confirm_abandon = true;
        cx.notify();
    }

    pub(crate) fn abandon_all_disasm_edits(&mut self, cx: &mut Context<Self>) {
        if let Some(b) = self.bundle_mut() {
            b.edits.clear();
        }
        self.changes_dialog_confirm_abandon = false;
        self.changes_dialog_open = false;
        cx.notify();
    }

    pub(crate) fn revert_disasm_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        vaddr: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(b) = self.bundle_mut() {
            b.edits.remove(&artifact, vaddr);
        }
        cx.notify();
    }

    /// Open a save-file dialog and write a patched copy of the
    /// currently-loaded bundle there. Driven by the Changes
    /// dialog's "Export N changes…" button.
    pub(crate) fn export_patched_bundle(&mut self, cx: &mut Context<Self>) {
        use std::collections::HashMap;
        let Some(bundle) = self.bundle() else { return };
        if bundle.edits.is_empty() {
            return;
        }
        // Build the EditMap up-front (cheap clone of edit
        // metadata) so the post-dialog continuation doesn't need
        // to reach back into the bundle.
        let mut edit_map: HashMap<glass_db::ArtifactId, Vec<glass_api::EditPatch>> =
            HashMap::new();
        for e in bundle.edits.entries() {
            edit_map.entry(e.artifact.clone()).or_default().push(
                glass_api::EditPatch {
                    vaddr: e.vaddr,
                    new_bytes: e.new_bytes.clone(),
                },
            );
        }
        // Re-load the source bundle from disk so the exporter
        // sees fresh bytes (the in-memory ParsedArtifact is the
        // source of truth for which file to patch, but the
        // exporter wants a Bundle handle anyway for the path).
        let source_path = self
            .source_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let suggested = patched_filename(&source_path);
        let dir = source_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let rx = {
            let app: &mut gpui::App = &mut *cx;
            app.prompt_for_new_path(&dir, Some(&suggested))
        };
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(out_path))) = rx.await else { return };
            // Flip the progress flag + close the dialog so the
            // overlay takes over.
            let _ = this.update(cx, |shell, cx| {
                shell.export_in_progress = true;
                shell.changes_dialog_open = false;
                shell.export_status = None;
                cx.notify();
            });
            // Animation pump: tick at ~30fps so the indeterminate
            // bar slides while the heavy work runs. Stops on its
            // own when `export_in_progress` flips false.
            {
                let this_pump = this.clone();
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(33))
                            .await;
                        let still_running = this_pump
                            .update(cx, |shell, cx| {
                                cx.notify();
                                shell.export_in_progress
                            })
                            .unwrap_or(false);
                        if !still_running {
                            break;
                        }
                    }
                })
                .detach();
            }
            // Re-open + export off the foreground thread. The
            // background_executor pool is the right home — gpui's
            // main runloop stays responsive while we splice the
            // archive.
            let edit_map_for_task = edit_map.clone();
            let source_path_for_task = source_path.clone();
            let out_path_for_task = out_path.clone();
            let summary = cx
                .background_executor()
                .spawn(async move {
                    match glass_api::open(&source_path_for_task) {
                        Ok(bundle) => match glass_api::export_to_path(
                            &bundle,
                            &edit_map_for_task,
                            &out_path_for_task,
                        ) {
                            Ok(()) => Ok(out_path_for_task),
                            Err(e) => Err(format!("{e:#}")),
                        },
                        Err(e) => Err(format!("re-open failed: {e:#}")),
                    }
                })
                .await;
            match &summary {
                Ok(p) => tracing::info!("exported patched bundle to {}", p.display()),
                Err(e) => tracing::warn!("export failed: {e}"),
            }
            let _ = this.update(cx, |shell, cx| {
                shell.export_in_progress = false;
                shell.export_status = Some(summary);
                cx.notify();
            });
        })
        .detach();
    }

    /// Close the dialog and jump to the edit's address. Picks
    /// the listing view for text-section addresses, the hex
    /// view for data-section addresses (matching where each
    /// kind of edit was originally staged).
    pub(crate) fn navigate_to_disasm_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        vaddr: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle() else { return };
        let target = bundle
            .text_section_for_addr(&artifact, vaddr)
            .map(|s| (s.to_string(), true))
            .or_else(|| {
                bundle
                    .data_section_for_addr(&artifact, vaddr)
                    .map(|s| (s.to_string(), false))
            });
        let Some((section, is_text)) = target else { return };
        self.changes_dialog_open = false;
        self.changes_dialog_confirm_abandon = false;
        if is_text {
            self.open_listing_in_new_tab(artifact, section, vaddr, cx);
        } else {
            self.open_hex_in_new_tab(artifact, section, vaddr, cx);
        }
    }

    fn bundle_mut(&mut self) -> Option<&mut crate::LoadedBundle> {
        if let crate::ShellState::Ready(b) = &mut self.state {
            Some(b)
        } else {
            None
        }
    }
}

/// Best-effort short ASCII preview of the string at `addr`, used
/// as the chip label for "References to ..." menu items pointing
/// at strings-section addresses. Returns `None` when the address
/// isn't in a strings section, when the byte before the address
/// isn't a NUL (i.e. it's mid-string), or when the string is
/// non-printable.
fn preview_string_at(
    bundle: &LoadedBundle,
    artifact: &glass_db::ArtifactId,
    addr: u64,
) -> Option<String> {
    let section_name = bundle.data_section_for_addr(artifact, addr)?;
    let section = bundle
        .data_sections
        .get(&(artifact.clone(), section_name.to_string()))?;
    let off = addr.checked_sub(section.base)? as usize;
    if off >= section.bytes.len() {
        return None;
    }
    let end = section.bytes[off..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| off + p)
        .unwrap_or(section.bytes.len());
    let raw = &section.bytes[off..end];
    if raw.is_empty() || !raw.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
        return None;
    }
    let s = std::str::from_utf8(raw).ok()?;
    Some(if s.len() > 40 {
        format!("{}…", &s[..40])
    } else {
        s.to_string()
    })
}

// ---- Xref → SearchEntry adapters ------------------------------------------
//
// These convert raw xref-index hits into displayable palette
// SearchEntries — same data shape as the bundle-wide cmd-F results,
// so the existing palette filter / activate machinery handles them.

fn build_native_xref_entries(
    bundle: &LoadedBundle,
    artifact: &glass_db::ArtifactId,
    target_addr: u64,
    idx: &crate::xref::NativeXrefs,
) -> Vec<SearchEntry> {
    let Some(per_art) = idx.get(artifact) else { return Vec::new() };
    let Some(sites) = per_art.get(&target_addr) else { return Vec::new() };
    sites
        .iter()
        .filter_map(|&site| {
            let section = bundle.text_section_for_addr(artifact, site)?;
            let sym = bundle
                .symbol_maps
                .get(artifact)
                .and_then(|sm| sm.covering(site));
            let display = match sym {
                Some(s) if s.address == site => s.display_name.clone(),
                Some(s) => format!("{}+0x{:x}", s.display_name, site - s.address),
                None => format!("0x{:x}", site),
            };
            let chip = format!("{section} · 0x{site:x}");
            Some(SearchEntry {
                display,
                chip,
                kind_glyph: "→",
                jump: SearchJump::Listing {
                    artifact: artifact.clone(),
                    section: section.to_string(),
                    addr: site,
                },
            })
        })
        .collect()
}

fn build_dex_caller_entries(
    bundle: &LoadedBundle,
    callee_key: &str,
    idx: &crate::xref::DexCallers,
) -> Vec<SearchEntry> {
    let Some(callers) = idx.get(callee_key) else { return Vec::new() };
    callers
        .iter()
        .filter_map(|(caller_key, line_offset)| {
            let class_jni = caller_key.split("->").next()?.to_string();
            // Resolve absolute line within the smali leaf:
            // .method header line + line_offset.
            let header_line = bundle
                .method_lines
                .get(caller_key)
                .map(|&(_, l)| l)
                .unwrap_or(0);
            let line = header_line + *line_offset as usize;
            let cls = class_jni
                .trim_start_matches('L')
                .trim_end_matches(';')
                .rsplit('/')
                .next()
                .unwrap_or(&class_jni);
            let method_name = caller_key
                .split("->")
                .nth(1)
                .and_then(|m| m.split('(').next())
                .unwrap_or("?");
            let display = format!("{cls}.{method_name}:{line_offset}");
            Some(SearchEntry {
                display,
                chip: "method".to_string(),
                kind_glyph: "ƒ",
                jump: SearchJump::SmaliMethodLine { class_jni, line },
            })
        })
        .collect()
}

fn build_dex_field_entries(
    bundle: &LoadedBundle,
    field_ref: &str,
    idx: &crate::xref::DexFieldRefs,
) -> Vec<SearchEntry> {
    let Some(touchers) = idx.get(field_ref) else { return Vec::new() };
    touchers
        .iter()
        .filter_map(|(method_key, line_offset)| {
            let class_jni = method_key.split("->").next()?.to_string();
            let header_line = bundle
                .method_lines
                .get(method_key)
                .map(|&(_, l)| l)
                .unwrap_or(0);
            let line = header_line + *line_offset as usize;
            let cls = class_jni
                .trim_start_matches('L')
                .trim_end_matches(';')
                .rsplit('/')
                .next()
                .unwrap_or(&class_jni);
            let method_name = method_key
                .split("->")
                .nth(1)
                .and_then(|m| m.split('(').next())
                .unwrap_or("?");
            let display = format!("{cls}.{method_name}:{line_offset}");
            Some(SearchEntry {
                display,
                chip: "field-ref".to_string(),
                kind_glyph: "ᕀ",
                jump: SearchJump::SmaliMethodLine { class_jni, line },
            })
        })
        .collect()
}

/// Pretty-print a 4-byte instruction at `addr`. Used by edit
/// staging to cache the disasm of the new bytes so the listing
/// renderer + Changes dialog don't have to re-decode on every
/// paint. PC-relative operands are resolved against `addr` so
/// branches show their real target.
pub(crate) fn decode_insn_pretty(bytes: &[u8; 4], addr: u64) -> String {
    decode_insn_pretty_with_symbols(bytes, addr, |_| None)
}

/// Variant that resolves address operands to symbol names via
/// the supplied closure. Used after staging an edit so the
/// "modified" row shows `bl decode_packet` instead of `bl 0x…`.
pub(crate) fn decode_insn_pretty_with_symbols<F>(
    bytes: &[u8; 4],
    addr: u64,
    symbol_for_address: F,
) -> String
where
    F: Fn(u64) -> Option<String>,
{
    let word = u32::from_le_bytes(*bytes);
    match armv8_encode::isa::aarch64::decode_instruction(addr, word) {
        Ok(insn) => {
            let mnem = insn.format_mnemonic();
            let ops = insn.format_operands_with_symbols(symbol_for_address);
            if ops.is_empty() {
                mnem
            } else {
                format!("{mnem} {ops}")
            }
        }
        Err(_) => format!(".word 0x{word:08x}"),
    }
}

/// Suggest a filename for the patched output. We keep the source
/// extension intact (so the patched output still looks like an
/// APK / IPA / `.so` to downstream tools) and insert
/// `-patched` before it.
fn patched_filename(source: &std::path::Path) -> String {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("patched");
    let ext = source.extension().and_then(|s| s.to_str()).unwrap_or("");
    if ext.is_empty() {
        format!("{stem}-patched")
    } else {
        format!("{stem}-patched.{ext}")
    }
}

/// Register names offered by the edit-mode autocomplete when
/// the cursor sits in a register slot. Order: zero registers,
/// stack pointers, then numeric W/X in ascending order. The
/// dropdown shows a prefix-filtered, capped subset.
const REGISTER_NAMES: &[&str] = &[
    "wzr", "xzr", "wsp", "sp",
    "w0", "w1", "w2", "w3", "w4", "w5", "w6", "w7", "w8", "w9",
    "w10", "w11", "w12", "w13", "w14", "w15", "w16", "w17", "w18", "w19",
    "w20", "w21", "w22", "w23", "w24", "w25", "w26", "w27", "w28", "w29", "w30",
    "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9",
    "x10", "x11", "x12", "x13", "x14", "x15", "x16", "x17", "x18", "x19",
    "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28", "x29", "x30",
];
