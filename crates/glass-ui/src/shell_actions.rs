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
            class_decl_edit: None,
            field_edit: None,
            method_edit: None,
            op_edit: None,
            annotation_stack: None,
            external_edit: None,
            device_manager: {
                // Honour an `adb_path` override from the window
                // settings, falling back to the default
                // discovery order. Cheap to construct — no I/O
                // beyond an `adb version` probe + a usbmuxd
                // socket open.
                let settings = glass_db::load_window_settings();
                let adb_override = settings
                    .adb_path
                    .as_ref()
                    .map(std::path::PathBuf::from);
                Arc::new(glass_device::DeviceManager::with_adb_override(
                    adb_override,
                ))
            },
            device_snapshot: Vec::new(),
            device_backend_status: glass_device::BackendStatus {
                adb: Err(glass_device::DeviceError::AdbNotFound),
                ios: Err(glass_device::DeviceError::IosBackendUnavailable(
                    "not probed yet".into(),
                )),
            },
            selected_device: None,
            device_picker_open: false,
            frida_probes: std::collections::HashMap::new(),
            injection_dialog: None,
            injection_progress: None,
            debug_dock: None,
            debug_dock_resize_anchor: None,
            traces_dialog_open: false,
            hooks_dialog_open: false,
            hook_editor_target: None,
            hook_editor_buffer: String::new(),
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
            // SmaliClass: pre-built line cache. If the user has
            // staged a typed edit for this class, re-render from the
            // modified `SmaliClass` rather than the original
            // `bundle.bodies[leaf]` string. Renderer falls back to the
            // pre-rendered body for unedited classes.
            TabKind::SmaliClass { class_jni } => {
                let class_jni = class_jni.clone();
                let Some(leaf) = self.tabs.get(active).and_then(|t| {
                    bundle.resolve(&t.kind.to_state())
                }) else {
                    return;
                };
                let tab = self.tabs.get_mut(active).unwrap();
                if tab.lines.is_none() {
                    let edited_text = bundle
                        .smali_classes
                        .iter()
                        .find_map(|((aid, jni), _)| {
                            if jni == &class_jni {
                                bundle
                                    .smali_edits
                                    .get(aid, jni)
                                    .map(|e| e.modified.to_smali())
                            } else {
                                None
                            }
                        });
                    let lines: Vec<SharedString> = if let Some(text) = edited_text {
                        text.lines()
                            .map(|l| SharedString::from(l.to_string()))
                            .collect()
                    } else {
                        bundle
                            .bodies
                            .get(leaf.0)
                            .map(|s| {
                                s.lines()
                                    .map(|l| SharedString::from(l.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    tab.scroll =
                        ListState::new(lines.len(), ListAlignment::Top, px(2000.));
                    tab.lines = Some(Arc::new(lines));
                }
                // Consume any pending deep-link line target now that
                // the body's line count is known (so scroll-to clamps
                // correctly). An explicit deep-link target wins over
                // the scroll-restore snapshot — the user asked to
                // jump.
                if let Some(line_no) = tab.pending_smali_scroll_line.take() {
                    let len = tab.lines.as_ref().map(|v| v.len()).unwrap_or(0);
                    if line_no < len {
                        scroll_into_view_with_context(&tab.scroll, line_no);
                        tab.selected_row = Some(line_no);
                    }
                    // A deep-link supersedes any prior restore.
                    tab.pending_scroll_restore = None;
                } else if let Some(offset) = tab.pending_scroll_restore.take() {
                    // Clamp item_ix to the new line count so a
                    // shortened body doesn't scroll past the end.
                    let len = tab.lines.as_ref().map(|v| v.len()).unwrap_or(0);
                    let clamped_ix = offset.item_ix.min(len.saturating_sub(1));
                    tab.scroll.scroll_to(ListOffset {
                        item_ix: clamped_ix,
                        offset_in_item: offset.offset_in_item,
                    });
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
                glass_db::AnnotationKey::OpIndex {
                    class_jni, method_decl, op_index,
                } => idx.at_op_index(
                    &format!("{class_jni}->{method_decl}"),
                    *op_index,
                ),
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
            // Translate the row's line offset into an op index
            // through the parsed SmaliMethod. Line offset 0 is
            // the `.method` header — keep that as a Method key
            // (no op). Anything else maps to an op via the
            // shared `line_offset_to_op_index` helper.
            //
            // Falls back to `MethodLine` only if we couldn't
            // find the SmaliMethod (e.g. a class that lifted
            // raw but didn't parse). In practice that's rare
            // and the fallback at least preserves the original
            // semantics for the duration of this session.
            let (key, existing) = if line_offset == 0 {
                let k = glass_db::AnnotationKey::Method(
                    class_jni.clone(),
                    method_decl.clone(),
                );
                let e = self
                    .bundle()
                    .and_then(|b| b.annotations.get(&artifact))
                    .and_then(|idx| idx.at_method(&method_key))
                    .cloned()
                    .unwrap_or_default();
                (k, e)
            } else {
                let op_index = self
                    .bundle()
                    .and_then(|b| {
                        b.smali_classes.iter().find_map(|((_aid, jni), c)| {
                            if jni == &class_jni {
                                c.methods.iter().find(|m| {
                                    format!(
                                        "{}{}",
                                        m.name,
                                        m.signature.to_jni()
                                    ) == method_decl
                                })
                            } else {
                                None
                            }
                        })
                    })
                    .and_then(|m| {
                        crate::annotations::line_offset_to_op_index(m, line_offset)
                    });
                match op_index {
                    Some(op_index) => {
                        let k = glass_db::AnnotationKey::OpIndex {
                            class_jni: class_jni.clone(),
                            method_decl: method_decl.clone(),
                            op_index,
                        };
                        let e = self
                            .bundle()
                            .and_then(|b| b.annotations.get(&artifact))
                            .and_then(|idx| {
                                idx.at_op_index(&method_key, op_index)
                            })
                            .cloned()
                            .unwrap_or_default();
                        (k, e)
                    }
                    None => {
                        let k = glass_db::AnnotationKey::MethodLine(
                            class_jni.clone(),
                            method_decl.clone(),
                            line_offset,
                        );
                        let e = self
                            .bundle()
                            .and_then(|b| b.annotations.get(&artifact))
                            .and_then(|idx| {
                                idx.at_method_line(&method_key, line_offset)
                            })
                            .cloned()
                            .unwrap_or_default();
                        (k, e)
                    }
                }
            };
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
                artifact: artifact.clone(),
                key,
                label: SharedString::from(format!("Clear annotation ({label})")),
            });
        }
        // If the active class has a staged structural edit, offer
        // a Revert. Walk smali_classes to find the matching artifact
        // — there's typically just one entry per jni, but APKs can
        // legally ship the same class in multiple DEX files.
        if let Some(bundle) = self.bundle() {
            let revert_targets: Vec<glass_db::ArtifactId> = bundle
                .smali_classes
                .iter()
                .filter_map(|((aid, jni), _)| {
                    if jni == &class_jni && bundle.smali_edits.get(aid, jni).is_some() {
                        Some(aid.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for aid in revert_targets {
                items.push(ContextMenuItem::RevertSmaliClassEdit {
                    artifact: aid,
                    class_jni: class_jni.clone(),
                    label: SharedString::from(format!("Revert class edit ({label})")),
                });
            }
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
    /// Right-click on a `.field` line in a smali listing.
    /// Always shows "References to field"; when the active class
    /// has a staged edit that touches this specific field, adds
    /// "Revert field edit" too.
    pub(crate) fn open_field_context_menu(
        &mut self,
        field_ref: String,
        display: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let label = SharedString::from(display.clone());
        let mut items =
            vec![ContextMenuItem::RefsToField { field_ref: field_ref.clone(), label }];
        // Field is edited if it appears in `edited_fields` for
        // the artifact that owns the active class. We need the
        // artifact id, the field's (name, sig), and a way to
        // know that the class is staged at all.
        if let Some((artifact, class_jni, name, sig)) =
            self.resolve_edited_field(&field_ref)
        {
            items.push(ContextMenuItem::RevertSmaliFieldEdit {
                artifact,
                class_jni,
                field_name: name,
                field_signature_jni: sig,
                label: SharedString::from(format!("Revert field edit ({display})")),
            });
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on a `.method` header in a smali listing.
    /// Shows the existing method options (callers + call-graph)
    /// plus, when the active class has a staged edit that
    /// touches this method, "Revert method edit".
    pub(crate) fn open_method_header_context_menu(
        &mut self,
        method_name: String,
        method_signature_jni: String,
        display: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let label = SharedString::from(display.clone());
        let Some(active) = self.active_tab else { return };
        let class_jni = match self.tabs.get(active).map(|t| &t.kind) {
            Some(TabKind::SmaliClass { class_jni }) => class_jni.clone(),
            _ => return,
        };
        // Pre-fetch the artifact so we can decide whether to
        // offer Revert. The other menu items don't need it.
        let artifact = self.bundle().and_then(|b| {
            b.smali_classes.keys().find_map(|(aid, jni)| {
                if jni == &class_jni { Some(aid.clone()) } else { None }
            })
        });
        let mut items: Vec<ContextMenuItem> = Vec::new();
        // Reuse the existing dex-callgraph / callers-of-method
        // entry points so the "Show call graph" menu item stays
        // available.
        let method_decl =
            format!("{method_name}{method_signature_jni}");
        items.push(ContextMenuItem::ShowDexCallGraph {
            class_jni: class_jni.clone(),
            method_decl: method_decl.clone(),
            label: label.clone(),
        });
        items.push(ContextMenuItem::CallersOfMethod {
            method_key: format!("{class_jni}->{method_decl}"),
            label: label.clone(),
        });
        if let Some(artifact) = artifact {
            if self
                .bundle()
                .and_then(|b| {
                    b.smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .map(|c| {
                            b.smali_edits
                                .edited_methods(&artifact, &class_jni, c)
                                .into_iter()
                                .any(|(n, s)| {
                                    n == method_name && s == method_signature_jni
                                })
                        })
                })
                .unwrap_or(false)
            {
                items.push(ContextMenuItem::RevertSmaliMethodEdit {
                    artifact: artifact.clone(),
                    class_jni: class_jni.clone(),
                    method_name: method_name.clone(),
                    method_signature_jni: method_signature_jni.clone(),
                    label: SharedString::from(format!(
                        "Revert method edit ({display})"
                    )),
                });
            }
            // Trace items — only show when the debug dock is
            // attached. Toggle between Start / Stop based on
            // current registry state. <clinit> is excluded
            // because Frida's Java.use can't hook static
            // initialisers.
            let dock_attached = self
                .debug_dock
                .as_ref()
                .map(|d| d.session.is_some())
                .unwrap_or(false);
            if dock_attached && method_name != "<clinit>" {
                let is_traced = self
                    .bundle()
                    .map(|b| {
                        b.traces.is_traced(
                            &artifact,
                            &class_jni,
                            &method_name,
                            &method_signature_jni,
                        )
                    })
                    .unwrap_or(false);
                if is_traced {
                    items.push(ContextMenuItem::StopTrace {
                        artifact: artifact.clone(),
                        class_jni: class_jni.clone(),
                        method_name: method_name.clone(),
                        method_signature_jni: method_signature_jni.clone(),
                        label: SharedString::from(display.clone()),
                    });
                } else {
                    items.push(ContextMenuItem::StartTrace {
                        artifact: artifact.clone(),
                        class_jni: class_jni.clone(),
                        method_name: method_name.clone(),
                        method_signature_jni: method_signature_jni.clone(),
                        label: SharedString::from(display.clone()),
                    });
                }
                // Hook items — same gating as traces.
                let is_hooked = self
                    .bundle()
                    .map(|b| {
                        b.hooks.is_hooked(
                            &artifact,
                            &class_jni,
                            &method_name,
                            &method_signature_jni,
                        )
                    })
                    .unwrap_or(false);
                if is_hooked {
                    items.push(ContextMenuItem::StopHook {
                        artifact,
                        class_jni,
                        method_name,
                        method_signature_jni,
                        label: SharedString::from(display),
                    });
                } else {
                    items.push(ContextMenuItem::StartHook {
                        artifact,
                        class_jni,
                        method_name,
                        method_signature_jni,
                        label: SharedString::from(display),
                    });
                }
            }
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Given a `field_ref` like `Lcom/Foo;->count:I`, find the
    /// owning artifact and return `(artifact, class_jni, name, sig)`
    /// when that field is currently edited. Returns `None` if
    /// the class isn't loaded, the ref doesn't parse, or the
    /// field isn't in the edited set.
    fn resolve_edited_field(
        &self,
        field_ref: &str,
    ) -> Option<(glass_db::ArtifactId, String, String, String)> {
        let (class_jni, rest) = field_ref.split_once("->")?;
        let (name, sig) = rest.split_once(':')?;
        let bundle = self.bundle()?;
        let (artifact, original) =
            bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
                if jni == class_jni { Some((aid.clone(), c.clone())) } else { None }
            })?;
        let edited = bundle
            .smali_edits
            .edited_fields(&artifact, class_jni, &original);
        if edited
            .into_iter()
            .any(|(n, s)| n == name && s == sig)
        {
            Some((artifact, class_jni.to_string(), name.to_string(), sig.to_string()))
        } else {
            None
        }
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
            ContextMenuItem::RevertSmaliClassEdit { artifact, class_jni, .. } => {
                self.revert_smali_class_edit(artifact, class_jni, cx);
            }
            ContextMenuItem::RevertSmaliFieldEdit {
                artifact,
                class_jni,
                field_name,
                field_signature_jni,
                ..
            } => {
                self.revert_smali_field_edit(
                    artifact,
                    class_jni,
                    field_name,
                    field_signature_jni,
                    cx,
                );
            }
            ContextMenuItem::RevertSmaliMethodEdit {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                self.revert_smali_method_edit(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    cx,
                );
            }
            ContextMenuItem::StartTrace {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                self.start_trace(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    cx,
                );
            }
            ContextMenuItem::StopTrace {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                self.stop_trace(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    cx,
                );
            }
            ContextMenuItem::StartHook {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                // Default action: LogOnly. User flips via the
                // Hooks dialog's Cycle button.
                self.start_hook(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    crate::hooks::HookAction::LogOnly,
                    cx,
                );
            }
            ContextMenuItem::StopHook {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                self.stop_hook(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    cx,
                );
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
            glass_db::AnnotationKey::OpIndex {
                class_jni, method_decl, op_index,
            } => format!("{class_jni}->{method_decl}#op{op_index}"),
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
            glass_db::AnnotationKey::OpIndex {
                class_jni,
                method_decl,
                op_index,
            } => {
                // Resolve the class's leaf + the method header line,
                // then render the method and walk to find the line
                // offset where op `op_index` lands.
                let method_key = format!("{class_jni}->{method_decl}");
                let Some((leaf, header_line)) =
                    bundle.method_lines.get(&method_key).copied()
                else {
                    if let Some(leaf) = bundle.resolve(&glass_db::TabState::SmaliClass {
                        class_jni: class_jni.clone(),
                        scroll_line: 0,
                    }) {
                        self.open_leaf(leaf, cx);
                    }
                    return;
                };
                // Find the SmaliMethod so we can map op index back
                // to a line offset.
                let target_line = bundle.smali_classes.iter().find_map(
                    |((_aid, jni), c)| {
                        if jni != &class_jni {
                            return None;
                        }
                        let m = c.methods.iter().find(|m| {
                            format!("{}{}", m.name, m.signature.to_jni())
                                == method_decl
                        })?;
                        crate::annotations::op_index_to_line_offset(m, op_index)
                            .map(|off| header_line + off as usize)
                    },
                );
                match target_line {
                    Some(line) => self.goto_smali_method(leaf, line, cx),
                    None => self.open_leaf(leaf, cx),
                }
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
                let row = rows.iter().position(|r| {
                    matches!(
                        r,
                        crate::HexRow::Bytes { address, .. }
                            if a >= *address && a < *address + 16
                    )
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
    /// Jump to the smali tab for `(artifact, class_jni)` and close
    /// the changes dialog. The artifact id isn't strictly needed —
    /// the bundle's leaf list keys on jni only — but the caller has
    /// it from the staged edit and we keep the same shape as
    /// `revert_smali_class_edit` for symmetry.
    pub(crate) fn navigate_to_smali_class(
        &mut self,
        _artifact: glass_db::ArtifactId,
        class_jni: String,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle() else { return };
        let leaf = bundle
            .resolve(&glass_db::TabState::SmaliClass {
                class_jni: class_jni.clone(),
                scroll_line: 0,
            });
        let Some(leaf) = leaf else { return };
        self.open_leaf(leaf, cx);
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.selected_row = Some(0);
                tab.pending_smali_scroll_line = Some(0);
            }
        }
        self.changes_dialog_open = false;
        cx.notify();
        self.save_state();
    }

    /// Navigate to a specific field or method inside a smali
    /// class. Opens the class's tab and scrolls so the matching
    /// `.field` / `.method` line is the selected row. Falls
    /// back to opening the class at line 0 if no matching member
    /// is found (e.g. the class lines haven't been built yet).
    pub(crate) fn navigate_to_smali_member(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        kind: SmaliMemberKind,
        cx: &mut Context<Self>,
    ) {
        // Reuse the existing open path to ensure the tab exists
        // and tab.lines is populated.
        self.navigate_to_smali_class(artifact, class_jni, cx);
        let Some(active) = self.active_tab else { return };
        // Find the matching `.field` or `.method` row in the
        // freshly-rendered line cache. If lines aren't built yet
        // (first paint), `ensure_active_tab_lines` will fill
        // them shortly — leaving row 0 selected is fine for
        // that frame.
        let Some(tab) = self.tabs.get(active) else { return };
        let Some(lines) = tab.lines.as_ref() else { return };
        let target_row = lines.iter().position(|line| {
            let t = line.trim_start();
            match &kind {
                SmaliMemberKind::Field { name, signature } => {
                    if !t.starts_with(".field ") {
                        return false;
                    }
                    // `name:sig` token must appear last on the line
                    // (before any `= initial`).
                    let head = match t.find(" = ") {
                        Some(eq) => &t[..eq],
                        None => t,
                    };
                    head.split_whitespace().last().is_some_and(|tok| {
                        tok == format!("{name}:{signature}").as_str()
                    })
                }
                SmaliMemberKind::Method { name, signature } => {
                    if !t.starts_with(".method ") {
                        return false;
                    }
                    // `nameSig` token must appear last on the line.
                    let token = t.split_whitespace().last().unwrap_or("");
                    token == format!("{name}{signature}")
                }
            }
        });
        if let Some(row) = target_row {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.selected_row = Some(row);
                tab.pending_smali_scroll_line = Some(row);
            }
            cx.notify();
            self.save_state();
        }
    }

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

    // ---- Class-declaration popover ----------------------------------

    /// If the active smali tab's selected row is part of the class
    /// declaration (`.class`, `.super`, `.implements`, `.source`),
    /// open the popover and return `true`. Otherwise return `false`
    /// so the caller can try another Enter behaviour.
    pub(crate) fn smali_open_class_decl_at_selection(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_tab else { return false };
        let Some(tab) = self.tabs.get(active) else { return false };
        if !matches!(tab.kind, TabKind::SmaliClass { .. }) {
            return false;
        }
        let Some(row) = tab.selected_row else { return false };
        let Some(lines) = tab.lines.as_ref() else { return false };
        let mask = crate::class_decl_popover::class_decl_row_mask(lines.as_slice());
        if !mask.get(row).copied().unwrap_or(false) {
            return false;
        }
        self.open_class_decl_edit(cx);
        true
    }

    /// Open the class-decl editor for the currently-active smali
    /// tab. Looks up the typed `SmaliClass` (preferring any staged
    /// edit) and seeds the popover state with its current values.
    pub(crate) fn open_class_decl_edit(&mut self, cx: &mut Context<Self>) {
        let Some(bundle) = self.bundle() else { return };
        let Some(active) = self.active_tab else { return };
        let Some(class_jni) = self.tabs.get(active).and_then(|t| match &t.kind {
            TabKind::SmaliClass { class_jni } => Some(class_jni.clone()),
            _ => None,
        }) else {
            return;
        };
        // Find the (artifact, jni) pair that owns this class — we
        // index `smali_classes` by both, so walk to recover the
        // artifact id for the picked jni.
        let owner = bundle
            .smali_classes
            .iter()
            .find_map(|((aid, jni), c)| {
                if jni == &class_jni {
                    Some((aid.clone(), c.clone()))
                } else {
                    None
                }
            });
        let Some((artifact, original)) = owner else { return };
        // If an edit is staged, seed from that instead so re-opening
        // shows the in-progress edits.
        let class = bundle
            .smali_edits
            .get(&artifact, &class_jni)
            .map(|e| e.modified.clone())
            .unwrap_or(original);
        self.class_decl_edit = Some(
            crate::class_decl_popover::ClassDeclEditState::from_class(artifact, class_jni, &class),
        );
        cx.notify();
    }

    pub(crate) fn cancel_class_decl_edit(&mut self, cx: &mut Context<Self>) {
        if self.class_decl_edit.take().is_some() {
            cx.notify();
        }
    }

    /// Save the in-progress class-decl form into the bundle's
    /// `smali_edits` registry. Invalidates the active smali tab's
    /// line cache so the next paint re-renders from the modified
    /// class.
    pub(crate) fn commit_class_decl_edit(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.class_decl_edit.take() else { return };
        if state.validate().is_err() {
            // Validation should already gate the Save button, but
            // a key-route can also reach here. Put the state back
            // and notify so the validation message shows.
            self.class_decl_edit = Some(state);
            cx.notify();
            return;
        }
        let artifact = state.artifact.clone();
        let class_jni = state.class_jni.clone();
        // Build the modified SmaliClass from the original. We have
        // to pull `original` out of `bundle.smali_classes` while
        // we hold an immutable view, then transition to a mutable
        // borrow to stage the edit. Clone to avoid the borrow split.
        let modified = {
            let Some(bundle) = self.bundle() else {
                self.class_decl_edit = Some(state);
                cx.notify();
                return;
            };
            let key = (artifact.clone(), class_jni.clone());
            let Some(original) = bundle.smali_classes.get(&key) else {
                self.class_decl_edit = Some(state);
                cx.notify();
                return;
            };
            state.build_modified(original)
        };
        if let Some(bundle) = self.bundle_mut() {
            bundle.smali_edits.insert(crate::smali_edits::SmaliEdit {
                key: crate::smali_edits::SmaliEditKey {
                    artifact,
                    class_jni: class_jni.clone(),
                },
                modified,
            });
        }
        // Invalidate the active smali tab's line cache so the
        // next paint re-renders from the modified class.
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                if matches!(tab.kind, TabKind::SmaliClass { .. }) {
                    tab.lines = None;
                }
            }
        }
        cx.notify();
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
            b.smali_edits.clear();
        }
        self.changes_dialog_confirm_abandon = false;
        self.changes_dialog_open = false;
        // Smali tabs cache their rendered lines — invalidate every
        // smali tab so the next paint re-renders from the original
        // class body. Snapshot scroll first so the user lands
        // where they were once the lines are rebuilt.
        for tab in &mut self.tabs {
            if matches!(tab.kind, TabKind::SmaliClass { .. }) {
                tab.pending_scroll_restore =
                    Some(tab.scroll.logical_scroll_top());
                tab.lines = None;
            }
        }
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

    /// Drop the staged smali-class edit for `(artifact, class_jni)`
    /// and invalidate any open smali tab for the same class so the
    /// next paint re-renders from the original body.
    pub(crate) fn revert_smali_class_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(b) = self.bundle_mut() {
            b.smali_edits.remove(&artifact, &class_jni);
        }
        for tab in &mut self.tabs {
            if let TabKind::SmaliClass { class_jni: jni } = &tab.kind {
                if jni == &class_jni {
                    tab.pending_scroll_restore =
                        Some(tab.scroll.logical_scroll_top());
                    tab.lines = None;
                }
            }
        }
        cx.notify();
    }

    /// Revert a single field's staged changes. Restores that
    /// field to its original lifted version inside the staged
    /// class. If the result happens to equal the original class
    /// in full, the class-level staged edit is dropped entirely.
    pub(crate) fn revert_smali_field_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        field_name: String,
        field_signature_jni: String,
        cx: &mut Context<Self>,
    ) {
        self.revert_member_edit(
            artifact,
            class_jni,
            cx,
            |original, modified| {
                let Some(orig_field) = original.fields.iter().find(|f| {
                    f.name == field_name
                        && f.signature.to_jni() == field_signature_jni
                }) else {
                    return;
                };
                if let Some(slot) = modified.fields.iter_mut().position(|f| {
                    f.name == field_name
                        && f.signature.to_jni() == field_signature_jni
                }) {
                    modified.fields[slot] = orig_field.clone();
                }
            },
        );
    }

    /// Revert a single method's staged changes. Symmetric to
    /// `revert_smali_field_edit`.
    pub(crate) fn revert_smali_method_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        cx: &mut Context<Self>,
    ) {
        self.revert_member_edit(
            artifact,
            class_jni,
            cx,
            |original, modified| {
                let Some(orig_method) = original.methods.iter().find(|m| {
                    m.name == method_name
                        && m.signature.to_jni() == method_signature_jni
                }) else {
                    return;
                };
                if let Some(slot) = modified.methods.iter_mut().position(|m| {
                    m.name == method_name
                        && m.signature.to_jni() == method_signature_jni
                }) {
                    modified.methods[slot] = orig_method.clone();
                }
            },
        );
    }

    /// Shared revert helper for fields and methods. Takes a
    /// closure that mutates the staged class to roll back one
    /// member, then either re-stages the trimmed class or drops
    /// the class edit entirely when it becomes a no-op.
    fn revert_member_edit<F>(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        cx: &mut Context<Self>,
        mutate: F,
    )
    where
        F: FnOnce(&smali::types::SmaliClass, &mut smali::types::SmaliClass),
    {
        let (original, mut modified) = {
            let Some(bundle) = self.bundle() else { return };
            let Some(original) = bundle
                .smali_classes
                .get(&(artifact.clone(), class_jni.clone()))
                .cloned()
            else {
                return;
            };
            let modified = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .unwrap_or_else(|| original.clone());
            (original, modified)
        };
        mutate(&original, &mut modified);
        // If the result is identity-equal to the original (via
        // writer output, same trick the class-decl tint uses),
        // drop the edit entirely. Otherwise re-stage.
        if original.to_smali() == modified.to_smali() {
            self.revert_smali_class_edit(artifact, class_jni, cx);
            return;
        }
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    // ---- Field popover -------------------------------------------------

    /// Enter-on-row entry point. If the selected row in the active
    /// smali tab is a `.field` line, opens the field popover and
    /// returns `true` so the caller short-circuits the normal Enter
    /// chain.
    pub(crate) fn smali_open_field_at_selection(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_tab else { return false };
        let Some(tab) = self.tabs.get(active) else { return false };
        if !matches!(tab.kind, TabKind::SmaliClass { .. }) {
            return false;
        }
        let Some(row) = tab.selected_row else { return false };
        let line = tab
            .lines
            .as_ref()
            .and_then(|v| v.get(row))
            .cloned();
        let Some(line) = line else { return false };
        if !crate::field_popover::line_is_field_decl(line.as_ref()) {
            return false;
        }
        self.open_field_edit_for_line(line.as_ref(), cx)
    }

    /// Double-click / Enter handler called once we already know the
    /// row text is a `.field` line. Parses the field name +
    /// signature out of the line text to identify which field in
    /// the active class to open. Returns whether the popover opened.
    pub(crate) fn open_field_edit_for_line(
        &mut self,
        line: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(bundle) = self.bundle() else { return false };
        let Some(active) = self.active_tab else { return false };
        let Some(class_jni) = self.tabs.get(active).and_then(|t| match &t.kind {
            TabKind::SmaliClass { class_jni } => Some(class_jni.clone()),
            _ => None,
        }) else {
            return false;
        };
        // Recover (name, signature) from the `.field` line —
        // shape is `.field <mods> <name>:<sig>[ = <init>]`.
        let Some((field_name, field_sig)) = parse_field_decl_line(line) else {
            return false;
        };
        // Find the owning artifact + class. Prefer the staged
        // edit so re-opening shows in-progress edits.
        let owner = bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
            if jni == &class_jni {
                Some((aid.clone(), c.clone()))
            } else {
                None
            }
        });
        let Some((artifact, original)) = owner else { return false };
        let class = bundle
            .smali_edits
            .get(&artifact, &class_jni)
            .map(|e| e.modified.clone())
            .unwrap_or(original);
        let field = class.fields.iter().find(|f| {
            f.name == field_name && f.signature.to_jni() == field_sig
        });
        let Some(field) = field else { return false };
        self.field_edit = Some(crate::field_popover::FieldEditState::from_field(
            artifact, class_jni, &class, field,
        ));
        cx.notify();
        true
    }

    pub(crate) fn cancel_field_edit(&mut self, cx: &mut Context<Self>) {
        if self.field_edit.take().is_some() {
            cx.notify();
        }
    }

    /// Save the in-progress field form into the bundle's
    /// `smali_edits` registry. Replaces the matching field on a
    /// clone of the parent class; if a class edit already exists,
    /// the new field overlay is layered on top of it.
    pub(crate) fn commit_field_edit(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.field_edit.take() else { return };
        if state.validate().is_err() {
            self.field_edit = Some(state);
            cx.notify();
            return;
        }
        let artifact = state.artifact.clone();
        let class_jni = state.class_jni.clone();
        let modified = {
            let Some(bundle) = self.bundle() else {
                self.field_edit = Some(state);
                cx.notify();
                return;
            };
            // Start from the staged class if any, else the original.
            let base = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .or_else(|| {
                    bundle
                        .smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .cloned()
                });
            let Some(base) = base else {
                self.field_edit = Some(state);
                cx.notify();
                return;
            };
            match state.build_modified(&base) {
                Some(c) => c,
                None => {
                    self.field_edit = Some(state);
                    cx.notify();
                    return;
                }
            }
        };
        if let Some(bundle) = self.bundle_mut() {
            bundle.smali_edits.insert(crate::smali_edits::SmaliEdit {
                key: crate::smali_edits::SmaliEditKey {
                    artifact,
                    class_jni: class_jni.clone(),
                },
                modified,
            });
        }
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                if matches!(tab.kind, TabKind::SmaliClass { .. }) {
                    tab.lines = None;
                }
            }
        }
        cx.notify();
    }

    // ---- Per-op inline editor -----------------------------------------

    /// Enter-on-row entry point. Opens the per-op editor when
    /// the selected row sits inside a method body (not on the
    /// `.method` header itself — that's the method-header
    /// popover's territory). Returns `true` to short-circuit
    /// the normal Enter chain.
    pub(crate) fn smali_open_op_edit_at_selection(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_tab else { return false };
        let Some(tab) = self.tabs.get(active) else { return false };
        if !matches!(tab.kind, TabKind::SmaliClass { .. }) {
            return false;
        }
        let Some(row) = tab.selected_row else { return false };
        self.open_op_edit_for_row(row, cx)
    }

    /// Open the inline op editor on `row_index` in the active
    /// smali tab. Returns whether it opened — `false` if the row
    /// isn't inside a method body, the method can't be resolved,
    /// or no class is loaded.
    pub(crate) fn open_op_edit_for_row(
        &mut self,
        row_index: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.op_edit.is_some() {
            // Already editing — don't stack edits.
            return false;
        }
        let Some(active) = self.active_tab else { return false };
        let Some(class_jni) = self.tabs.get(active).and_then(|t| match &t.kind {
            TabKind::SmaliClass { class_jni } => Some(class_jni.clone()),
            _ => None,
        }) else {
            return false;
        };
        let Some(tab) = self.tabs.get(active) else { return false };
        let Some(lines) = tab.lines.as_ref() else { return false };
        let Some(line_text) = lines.get(row_index).cloned() else { return false };
        // Find the enclosing `.method` row. If the user clicked
        // the header itself, defer to the method-header popover
        // by returning false.
        let mut header_row = None;
        for j in (0..=row_index).rev() {
            let Some(l) = lines.get(j) else { continue };
            let t = l.trim_start();
            if t.starts_with(".method ") {
                header_row = Some(j);
                break;
            }
            if t.starts_with(".end method") {
                // Past the previous method's tail — not in a body.
                return false;
            }
        }
        let Some(header_row) = header_row else { return false };
        if row_index == header_row {
            return false;
        }
        // Don't open an editor on `.end method` — that line is
        // structural; the user can use the method popover or the
        // external editor for big changes.
        if line_text.trim_start().starts_with(".end method") {
            return false;
        }
        let line_offset_within_method = row_index - header_row;
        // Resolve the artifact + (name, sig) of the method via
        // the row's scope mask. Cheap to recompute here so we
        // don't have to thread the mask through the call.
        let scope = crate::smali_row_scope::compute(lines.as_slice());
        let Some(crate::smali_row_scope::RowScope::Method { name, signature }) =
            scope.get(row_index)
        else {
            return false;
        };
        let method_name = name.clone();
        let method_signature_jni = signature.clone();
        // Recover the artifact id from `smali_classes`.
        let Some(bundle) = self.bundle() else { return false };
        let Some(artifact) = bundle.smali_classes.keys().find_map(|(aid, jni)| {
            if jni == &class_jni { Some(aid.clone()) } else { None }
        }) else {
            return false;
        };
        let initial = line_text.trim_start_matches(['\t', ' ']).to_string();
        self.op_edit = Some(crate::op_editor::OpEditState {
            artifact,
            class_jni,
            method_name,
            method_signature_jni,
            row_index,
            line_offset_within_method,
            is_new_line: false,
            input: crate::text_input::TextInput::from_text(initial),
            error: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
        });
        cx.notify();
        self.refresh_op_edit_suggestions(cx);
        true
    }

    pub(crate) fn cancel_op_edit(&mut self, cx: &mut Context<Self>) {
        if self.op_edit.take().is_some() {
            cx.notify();
        }
    }

    /// Common path for Enter (replace in place) and Cmd-Enter
    /// (insert below). `insert_after` distinguishes the two.
    /// Shift every `OpIndex` annotation on `(artifact, class,
    /// method_decl)` whose `op_index >= shift_from` by `delta`.
    /// Used by the per-op editor when inserting or deleting an
    /// op shifts later ops' indices.
    ///
    /// Persists through `glass_db` and refreshes the in-memory
    /// index for the artifact in one go.
    fn shift_op_index_annotations(
        &mut self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        method_decl: &str,
        shift_from: u32,
        delta: i64,
        _cx: &mut Context<Self>,
    ) {
        let Some(db) = self.db_ref() else { return };
        let Ok(entries) = db.load_annotations(artifact) else { return };
        for (key, ann) in entries {
            if let glass_db::AnnotationKey::OpIndex {
                class_jni: ck,
                method_decl: mk,
                op_index,
            } = &key
            {
                if ck != class_jni || mk != method_decl || *op_index < shift_from {
                    continue;
                }
                let new_idx_i64 = *op_index as i64 + delta;
                if new_idx_i64 < 0 {
                    // Deletion swallowed the slot — drop the
                    // annotation entirely.
                    db.clear_annotation(artifact.clone(), key);
                    continue;
                }
                let new_idx = new_idx_i64 as u32;
                let new_key = glass_db::AnnotationKey::OpIndex {
                    class_jni: ck.clone(),
                    method_decl: mk.clone(),
                    op_index: new_idx,
                };
                // Order matters: clear old before set new, in
                // case shift collapses two distinct keys into the
                // same one (shouldn't happen with a single
                // insertion, but cheap to be safe).
                db.clear_annotation(artifact.clone(), key);
                db.set_annotation(artifact.clone(), new_key, ann);
            }
        }
        let _ = db.flush();
        // Rebuild this artifact's in-memory index from the
        // freshly-updated DB so the smali tab re-renders with
        // the right dots / colours.
        let _ = self.refresh_artifact_annotations(artifact);
    }

    fn finish_op_edit(&mut self, insert_after: bool, cx: &mut Context<Self>) {
        let Some(state) = self.op_edit.as_ref() else { return };
        let artifact = state.artifact.clone();
        let class_jni = state.class_jni.clone();
        let method_name = state.method_name.clone();
        let method_signature_jni = state.method_signature_jni.clone();
        let line_offset = state.line_offset_within_method;
        let row_index = state.row_index;
        let new_line = state.input.text().to_string();
        // Locate the original method on the staged-or-original
        // class, splice the user's line in, round-trip via a
        // synthetic class, then write the new ops back onto the
        // real class.
        let mut staged = match self.staged_or_original_class(&artifact, &class_jni) {
            Some(c) => c,
            None => return,
        };
        let method_idx = match staged.methods.iter().position(|m| {
            m.name == method_name
                && m.signature.to_jni() == method_signature_jni
        }) {
            Some(i) => i,
            None => return,
        };
        let method_text = staged.methods[method_idx].to_string();
        let new_body = crate::op_editor::splice_method_body(
            &method_text,
            line_offset,
            &new_line,
            insert_after,
        );
        let wrapper = crate::op_editor::wrap_in_synthetic_class(&new_body, &class_jni);
        let parsed = match glass_api::parse_smali_class(&wrapper) {
            Ok(c) => c,
            Err(e) => {
                self.set_op_edit_error(format!("{e:#}"), cx);
                return;
            }
        };
        // The synthetic class contains exactly one method.
        let Some(new_method) = parsed.methods.into_iter().next() else {
            self.set_op_edit_error(
                "parsed body had no methods (smali parser quirk?)".to_string(),
                cx,
            );
            return;
        };
        // Preserve the original method's identifying metadata so
        // subsequent lookups by (name, signature) still resolve.
        let original = staged.methods[method_idx].clone();
        let old_op_count = original.ops.len();
        let new_op_count = new_method.ops.len();
        let method_decl = format!("{}{}", original.name, original.signature.to_jni());
        // Locate the op the edit landed on *before* we move
        // `original` into the assignment below — we need its
        // unchanged shape to map the user's line offset back
        // to an op index.
        let edited_op_index = crate::annotations::line_offset_to_op_index(
            &original,
            line_offset as u32,
        )
        .unwrap_or(0);
        staged.methods[method_idx] = smali::types::SmaliMethod {
            name: original.name,
            modifiers: original.modifiers,
            constructor: original.constructor,
            signature: original.signature,
            locals: new_method.locals,
            registers: new_method.registers.or(original.registers),
            params: original.params,
            annotations: original.annotations,
            ops: new_method.ops,
        };
        // Re-key OpIndex annotations whose indices shifted. For
        // a pure replace, `delta == 0` and there's nothing to
        // do. Insert-after raises the count by 1; a future
        // delete path would lower it.
        let delta: i64 = new_op_count as i64 - old_op_count as i64;
        if delta != 0 {
            // Insert-after pushes everything from edited_op + 1
            // onwards by `delta`. Delete would remove the slot
            // at edited_op itself; same shift formula.
            let shift_from = if delta > 0 {
                edited_op_index.saturating_add(1)
            } else {
                edited_op_index
            };
            self.shift_op_index_annotations(
                &artifact,
                &class_jni,
                &method_decl,
                shift_from,
                delta,
                cx,
            );
        }
        self.stage_smali_class_edit(artifact, class_jni, staged, cx);
        // Drop the editor state — the row underneath has just
        // been re-rendered by stage_smali_class_edit invalidating
        // tab.lines, so any inline TextInput would be paired
        // against stale row indices.
        if !insert_after {
            self.op_edit = None;
            cx.notify();
            return;
        }
        // For Cmd-Enter, re-open the editor on the new (blank)
        // line one row below the one we just edited. Lines are
        // re-rendered, so the row index advances by exactly one.
        let new_row = row_index + 1;
        if let Some(state) = self.op_edit.as_mut() {
            state.row_index = new_row;
            state.line_offset_within_method = line_offset + 1;
            state.is_new_line = true;
            state.input = crate::text_input::TextInput::new();
            state.error = None;
            state.suggestions.clear();
            state.suggestion_selected = 0;
        }
        cx.notify();
        self.refresh_op_edit_suggestions(cx);
    }

    pub(crate) fn commit_op_edit(&mut self, cx: &mut Context<Self>) {
        self.finish_op_edit(false, cx);
    }

    pub(crate) fn commit_op_edit_and_insert_below(&mut self, cx: &mut Context<Self>) {
        self.finish_op_edit(true, cx);
    }

    /// Recompute the autocomplete suggestion list for the
    /// editor's current cursor. Called after every keystroke and
    /// cursor move. Cheap enough to do synchronously — the
    /// largest source (per-bundle class list) is filtered by
    /// prefix and capped at 12 entries.
    pub(crate) fn refresh_op_edit_suggestions(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.op_edit.as_ref() else { return };
        let ctx = crate::op_editor::classify_cursor(
            state.input.text(),
            state.input.cursor(),
        );
        let suggestions = self.build_op_suggestions(ctx, &state.class_jni);
        if let Some(state) = self.op_edit.as_mut() {
            state.suggestions = suggestions;
            if state.suggestion_selected >= state.suggestions.len() {
                state.suggestion_selected = 0;
            }
        }
        cx.notify();
    }

    /// Build a suggestion list for `ctx`. Pure: doesn't touch
    /// `self.op_edit`. Bundle-aware sources walk the loaded
    /// classes / methods / fields; static sources (opcodes,
    /// registers) are hard-coded.
    fn build_op_suggestions(
        &self,
        ctx: crate::op_editor::OpCursorContext,
        active_class_jni: &str,
    ) -> Vec<crate::op_editor::OpSuggestion> {
        use crate::op_editor::{OpCursorContext, OpSuggestion, OpSuggestionKind};
        const MAX: usize = 50;
        match ctx {
            OpCursorContext::None => Vec::new(),
            OpCursorContext::Opcode { partial, replace_range } => {
                crate::op_editor::OPCODE_LIST
                    .iter()
                    .filter(|m| m.starts_with(&partial))
                    .take(MAX)
                    .map(|m| OpSuggestion {
                        label: SharedString::from(m.to_string()),
                        detail: SharedString::from("opcode"),
                        commit_text: m.to_string(),
                        replace_range,
                        kind: OpSuggestionKind::Opcode,
                    })
                    .collect()
            }
            OpCursorContext::Register { partial, replace_range } => {
                let mut out = Vec::new();
                for i in 0..=15u8 {
                    let name = format!("v{i}");
                    if name.starts_with(&partial) {
                        out.push(OpSuggestion {
                            label: SharedString::from(name.clone()),
                            detail: SharedString::from("local"),
                            commit_text: name,
                            replace_range,
                            kind: OpSuggestionKind::Register,
                        });
                    }
                }
                for i in 0..=7u8 {
                    let name = format!("p{i}");
                    if name.starts_with(&partial) {
                        out.push(OpSuggestion {
                            label: SharedString::from(name.clone()),
                            detail: SharedString::from("param"),
                            commit_text: name,
                            replace_range,
                            kind: OpSuggestionKind::Register,
                        });
                    }
                }
                out
            }
            OpCursorContext::Type { partial, replace_range } => {
                let Some(bundle) = self.bundle() else { return Vec::new() };
                let mut out: Vec<OpSuggestion> = bundle
                    .smali_classes
                    .keys()
                    .filter_map(|(_aid, jni)| {
                        if jni.starts_with(&partial) {
                            Some(OpSuggestion {
                                label: SharedString::from(jni.clone()),
                                detail: SharedString::from("internal class"),
                                commit_text: jni.clone(),
                                replace_range,
                                kind: OpSuggestionKind::Type,
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                // Common Java/Android types as a fallback — these
                // aren't in the loaded DEX but typed-by-hand
                // refs to them are very common.
                for stock in COMMON_EXTERNAL_TYPES {
                    if stock.starts_with(&partial)
                        && !out.iter().any(|s| s.commit_text == *stock)
                    {
                        out.push(OpSuggestion {
                            label: SharedString::from(*stock),
                            detail: SharedString::from("stdlib"),
                            commit_text: stock.to_string(),
                            replace_range,
                            kind: OpSuggestionKind::Type,
                        });
                    }
                }
                out.sort_by(|a, b| a.label.cmp(&b.label));
                out.truncate(MAX);
                out
            }
            OpCursorContext::MethodRef {
                class_jni,
                partial,
                replace_range,
            } => {
                let class = class_jni
                    .as_deref()
                    .unwrap_or(active_class_jni);
                self.suggestions_for_method_ref(class, &partial, replace_range, MAX)
            }
            OpCursorContext::FieldRef {
                class_jni,
                partial,
                replace_range,
            } => {
                let class = class_jni
                    .as_deref()
                    .unwrap_or(active_class_jni);
                self.suggestions_for_field_ref(class, &partial, replace_range, MAX)
            }
        }
    }

    fn suggestions_for_method_ref(
        &self,
        class_jni: &str,
        partial: &str,
        replace_range: (usize, usize),
        max: usize,
    ) -> Vec<crate::op_editor::OpSuggestion> {
        use crate::op_editor::{OpSuggestion, OpSuggestionKind};
        let Some(bundle) = self.bundle() else { return Vec::new() };
        // Find the class — prefer the staged version so newly
        // added methods show up immediately.
        let class = bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
            if jni == class_jni {
                let staged = bundle.smali_edits.get(aid, jni).map(|e| e.modified.clone());
                Some(staged.unwrap_or_else(|| c.clone()))
            } else {
                None
            }
        });
        let Some(class) = class else { return Vec::new() };
        class
            .methods
            .iter()
            .filter_map(|m| {
                let display = format!("{}{}", m.name, m.signature.to_jni());
                if display.starts_with(partial) {
                    Some(OpSuggestion {
                        label: SharedString::from(display.clone()),
                        detail: SharedString::from("method"),
                        commit_text: display,
                        replace_range,
                        kind: OpSuggestionKind::MethodRef,
                    })
                } else {
                    None
                }
            })
            .take(max)
            .collect()
    }

    fn suggestions_for_field_ref(
        &self,
        class_jni: &str,
        partial: &str,
        replace_range: (usize, usize),
        max: usize,
    ) -> Vec<crate::op_editor::OpSuggestion> {
        use crate::op_editor::{OpSuggestion, OpSuggestionKind};
        let Some(bundle) = self.bundle() else { return Vec::new() };
        let class = bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
            if jni == class_jni {
                let staged = bundle.smali_edits.get(aid, jni).map(|e| e.modified.clone());
                Some(staged.unwrap_or_else(|| c.clone()))
            } else {
                None
            }
        });
        let Some(class) = class else { return Vec::new() };
        class
            .fields
            .iter()
            .filter_map(|f| {
                let display = format!("{}:{}", f.name, f.signature.to_jni());
                if display.starts_with(partial) {
                    Some(OpSuggestion {
                        label: SharedString::from(display.clone()),
                        detail: SharedString::from("field"),
                        commit_text: display,
                        replace_range,
                        kind: OpSuggestionKind::FieldRef,
                    })
                } else {
                    None
                }
            })
            .take(max)
            .collect()
    }

    /// Accept the currently-highlighted suggestion. Splices its
    /// `commit_text` into the input over the suggestion's
    /// `replace_range`. No-op if there's no suggestion list.
    pub(crate) fn accept_op_edit_suggestion(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.op_edit.as_mut() else { return };
        let Some(sugg) = state.suggestions.get(state.suggestion_selected).cloned()
        else {
            return;
        };
        let text = state.input.text().to_string();
        let (start, end) = sugg.replace_range;
        let start = start.min(text.len());
        let end = end.min(text.len()).max(start);
        let mut new_text = String::with_capacity(
            text.len() - (end - start) + sugg.commit_text.len(),
        );
        new_text.push_str(&text[..start]);
        new_text.push_str(&sugg.commit_text);
        new_text.push_str(&text[end..]);
        let new_cursor = start + sugg.commit_text.len();
        state.input.set_text(new_text);
        state.input.set_cursor_pos(new_cursor, false);
        state.error = None;
        cx.notify();
        // Refresh — the new cursor may have entered a different
        // context (e.g. after picking an opcode the next slot is
        // a register).
        self.refresh_op_edit_suggestions(cx);
    }

    /// Click handler for the dropdown rows — selects the row
    /// and accepts it in one shot.
    pub(crate) fn click_op_edit_suggestion(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.op_edit.as_mut() {
            if index < state.suggestions.len() {
                state.suggestion_selected = index;
            }
        }
        self.accept_op_edit_suggestion(cx);
    }

    fn set_op_edit_error(&mut self, msg: String, cx: &mut Context<Self>) {
        if let Some(state) = self.op_edit.as_mut() {
            state.error = Some(msg);
        }
        cx.notify();
    }

    /// Helper: return a clone of the staged class for `(artifact,
    /// class_jni)` if any, else the original lifted class.
    /// Returns `None` if neither is loaded.
    fn staged_or_original_class(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
    ) -> Option<smali::types::SmaliClass> {
        let bundle = self.bundle()?;
        bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            })
    }

    // ---- Method header popover ----------------------------------------

    /// Enter-on-row entry point. If the selected row in the active
    /// smali tab is a `.method` header, opens the method popover
    /// and returns `true` so the caller short-circuits the normal
    /// Enter chain.
    pub(crate) fn smali_open_method_at_selection(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_tab else { return false };
        let Some(tab) = self.tabs.get(active) else { return false };
        if !matches!(tab.kind, TabKind::SmaliClass { .. }) {
            return false;
        }
        let Some(row) = tab.selected_row else { return false };
        let line = tab.lines.as_ref().and_then(|v| v.get(row)).cloned();
        let Some(line) = line else { return false };
        if !crate::method_popover::line_is_method_decl(line.as_ref()) {
            return false;
        }
        self.open_method_edit_for_line(line.as_ref(), cx)
    }

    /// Double-click / Enter handler called once we already know
    /// the row text is a `.method` line. Parses the method name +
    /// signature out of the line text and opens the popover.
    /// Returns whether it opened.
    pub(crate) fn open_method_edit_for_line(
        &mut self,
        line: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(bundle) = self.bundle() else { return false };
        let Some(active) = self.active_tab else { return false };
        let Some(class_jni) = self.tabs.get(active).and_then(|t| match &t.kind {
            TabKind::SmaliClass { class_jni } => Some(class_jni.clone()),
            _ => None,
        }) else {
            return false;
        };
        let Some((method_name, method_sig)) = parse_method_decl_line(line) else {
            return false;
        };
        let owner = bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
            if jni == &class_jni {
                Some((aid.clone(), c.clone()))
            } else {
                None
            }
        });
        let Some((artifact, original)) = owner else { return false };
        let class = bundle
            .smali_edits
            .get(&artifact, &class_jni)
            .map(|e| e.modified.clone())
            .unwrap_or(original);
        let method = class.methods.iter().find(|m| {
            m.name == method_name && m.signature.to_jni() == method_sig
        });
        let Some(method) = method else { return false };
        self.method_edit = Some(crate::method_popover::MethodEditState::from_method(
            artifact, class_jni, &class, method,
        ));
        cx.notify();
        true
    }

    pub(crate) fn cancel_method_edit(&mut self, cx: &mut Context<Self>) {
        if self.method_edit.take().is_some() {
            cx.notify();
        }
    }

    /// Save the in-progress method header form. Replaces the
    /// matching method in the staged-or-original class with the
    /// new metadata; body / params / annotations are preserved
    /// from the original.
    pub(crate) fn commit_method_edit(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.method_edit.take() else { return };
        if state.validate().is_err() {
            self.method_edit = Some(state);
            cx.notify();
            return;
        }
        let artifact = state.artifact.clone();
        let class_jni = state.class_jni.clone();
        let modified = {
            let Some(bundle) = self.bundle() else {
                self.method_edit = Some(state);
                cx.notify();
                return;
            };
            let base = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .or_else(|| {
                    bundle
                        .smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .cloned()
                });
            let Some(base) = base else {
                self.method_edit = Some(state);
                cx.notify();
                return;
            };
            match state.build_modified(&base) {
                Some(c) => c,
                None => {
                    self.method_edit = Some(state);
                    cx.notify();
                    return;
                }
            }
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    // ---- Annotation editor --------------------------------------------

    /// Open the annotation editor against a class-level annotation.
    /// `index == None` means the user is adding a brand-new
    /// annotation (Save will push); `Some(i)` edits the existing
    /// annotation at `class.annotations[i]`.
    pub(crate) fn open_class_annotation_editor(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        // Source the existing annotation — prefer the staged class
        // so re-opens reflect prior edits.
        let frame = {
            let Some(bundle) = self.bundle() else { return };
            let class = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .or_else(|| {
                    bundle
                        .smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .cloned()
                });
            let Some(class) = class else { return };
            match index {
                Some(i) => match class.annotations.get(i) {
                    Some(a) => crate::annotation_popover::AnnotationFrame::from_annotation(
                        a, None,
                    ),
                    None => return,
                },
                None => crate::annotation_popover::AnnotationFrame::blank(None),
            }
        };
        self.annotation_stack = Some(crate::annotation_popover::AnnotationStack {
            root_target: crate::annotation_popover::AnnotationTarget::ClassAnnotation {
                artifact,
                class_jni,
                index,
            },
            frames: vec![frame],
        });
        cx.notify();
    }

    /// Open the annotation editor against a field annotation.
    pub(crate) fn open_field_annotation_editor(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        field_name: String,
        field_signature_jni: String,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let frame = {
            let Some(bundle) = self.bundle() else { return };
            let class = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .or_else(|| {
                    bundle
                        .smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .cloned()
                });
            let Some(class) = class else { return };
            let field = class.fields.iter().find(|f| {
                f.name == field_name && f.signature.to_jni() == field_signature_jni
            });
            let Some(field) = field else { return };
            match index {
                Some(i) => match field.annotations.get(i) {
                    Some(a) => crate::annotation_popover::AnnotationFrame::from_annotation(
                        a, None,
                    ),
                    None => return,
                },
                None => crate::annotation_popover::AnnotationFrame::blank(None),
            }
        };
        self.annotation_stack = Some(crate::annotation_popover::AnnotationStack {
            root_target: crate::annotation_popover::AnnotationTarget::FieldAnnotation {
                artifact,
                class_jni,
                field_name,
                field_signature_jni,
                index,
            },
            frames: vec![frame],
        });
        cx.notify();
    }

    /// Open the annotation editor against a method annotation.
    pub(crate) fn open_method_annotation_editor(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let frame = {
            let Some(bundle) = self.bundle() else { return };
            let class = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .or_else(|| {
                    bundle
                        .smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .cloned()
                });
            let Some(class) = class else { return };
            let method = class.methods.iter().find(|m| {
                m.name == method_name
                    && m.signature.to_jni() == method_signature_jni
            });
            let Some(method) = method else { return };
            match index {
                Some(i) => match method.annotations.get(i) {
                    Some(a) => crate::annotation_popover::AnnotationFrame::from_annotation(
                        a, None,
                    ),
                    None => return,
                },
                None => crate::annotation_popover::AnnotationFrame::blank(None),
            }
        };
        self.annotation_stack = Some(crate::annotation_popover::AnnotationStack {
            root_target: crate::annotation_popover::AnnotationTarget::MethodAnnotation {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                index,
            },
            frames: vec![frame],
        });
        cx.notify();
    }

    /// Push a SubAnnotation frame for `elements[elem_index]` on the
    /// top-most frame. Seeded from the snapshot already stored
    /// there; saving the child overwrites the snapshot.
    pub(crate) fn push_sub_annotation_frame(
        &mut self,
        elem_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(stack) = self.annotation_stack.as_mut() else { return };
        let Some(top) = stack.frames.last() else { return };
        let Some(elem) = top.elements.get(elem_index) else { return };
        let snapshot = match &elem.value {
            crate::annotation_popover::AnnotationValueDraft::SubAnnotation(s) => {
                (**s).clone()
            }
            _ => return,
        };
        let frame = crate::annotation_popover::AnnotationFrame::from_annotation(
            &snapshot,
            Some(elem_index),
        );
        stack.frames.push(frame);
        cx.notify();
    }

    /// Cancel the top-most annotation frame. If it's a child,
    /// returns control to its parent. If it's the root, closes the
    /// whole editor without writing anything.
    pub(crate) fn cancel_annotation_frame(&mut self, cx: &mut Context<Self>) {
        let Some(stack) = self.annotation_stack.as_mut() else { return };
        stack.frames.pop();
        if stack.frames.is_empty() {
            self.annotation_stack = None;
        }
        cx.notify();
    }

    /// Save the top-most frame.
    ///
    /// * Child frame — copy its draft back into the parent frame's
    ///   `elements[parent_element_index].value` as a fresh
    ///   `SubAnnotation` snapshot, then pop.
    /// * Root frame — write the assembled `SmaliAnnotation` through
    ///   the stack's `root_target` into the bundle's smali edits.
    pub(crate) fn commit_annotation_frame(&mut self, cx: &mut Context<Self>) {
        let Some(stack) = self.annotation_stack.as_mut() else { return };
        let Some(top) = stack.frames.last() else { return };
        if top.validate().is_err() {
            cx.notify();
            return;
        }
        if stack.frames.len() > 1 {
            // Child: copy snapshot up into parent.
            let assembled = top.to_annotation();
            let parent_idx = top.parent_element_index;
            stack.frames.pop();
            if let (Some(parent_frame), Some(elem_idx)) =
                (stack.frames.last_mut(), parent_idx)
            {
                if let Some(elem) = parent_frame.elements.get_mut(elem_idx) {
                    elem.value =
                        crate::annotation_popover::AnnotationValueDraft::SubAnnotation(
                            Box::new(assembled),
                        );
                }
            }
            cx.notify();
            return;
        }
        // Root: write into the bundle.
        let assembled = top.to_annotation();
        let target = stack.root_target.clone();
        self.annotation_stack = None;
        self.apply_annotation_root(target, assembled, cx);
    }

    /// Apply a freshly-assembled annotation back into the bundle's
    /// staged class. Splits class / field paths so each is plainly
    /// readable.
    fn apply_annotation_root(
        &mut self,
        target: crate::annotation_popover::AnnotationTarget,
        annotation: smali::types::SmaliAnnotation,
        cx: &mut Context<Self>,
    ) {
        use crate::annotation_popover::AnnotationTarget;
        match target {
            AnnotationTarget::ClassAnnotation { artifact, class_jni, index } => {
                self.write_class_annotation(artifact, class_jni, index, annotation, cx);
            }
            AnnotationTarget::FieldAnnotation {
                artifact,
                class_jni,
                field_name,
                field_signature_jni,
                index,
            } => {
                self.write_field_annotation(
                    artifact,
                    class_jni,
                    field_name,
                    field_signature_jni,
                    index,
                    annotation,
                    cx,
                );
            }
            AnnotationTarget::MethodAnnotation {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                index,
            } => {
                self.write_method_annotation(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    index,
                    annotation,
                    cx,
                );
            }
        }
    }

    fn write_class_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        index: Option<usize>,
        annotation: smali::types::SmaliAnnotation,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            match index {
                Some(i) => {
                    if i < class.annotations.len() {
                        class.annotations[i] = annotation;
                    } else {
                        class.annotations.push(annotation);
                    }
                }
                None => class.annotations.push(annotation),
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    fn write_field_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        field_name: String,
        field_signature_jni: String,
        index: Option<usize>,
        annotation: smali::types::SmaliAnnotation,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if let Some(field) = class.fields.iter_mut().find(|f| {
                f.name == field_name && f.signature.to_jni() == field_signature_jni
            }) {
                match index {
                    Some(i) => {
                        if i < field.annotations.len() {
                            field.annotations[i] = annotation;
                        } else {
                            field.annotations.push(annotation);
                        }
                    }
                    None => field.annotations.push(annotation),
                }
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    fn write_method_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        index: Option<usize>,
        annotation: smali::types::SmaliAnnotation,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if let Some(method) = class.methods.iter_mut().find(|m| {
                m.name == method_name && m.signature.to_jni() == method_signature_jni
            }) {
                match index {
                    Some(i) => {
                        if i < method.annotations.len() {
                            method.annotations[i] = annotation;
                        } else {
                            method.annotations.push(annotation);
                        }
                    }
                    None => method.annotations.push(annotation),
                }
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    /// Helper: take the staged-or-original SmaliClass for
    /// `(artifact, class_jni)`, hand it to `f` for mutation, and
    /// return the mutated copy. Returns `None` if no such class is
    /// loaded.
    fn with_staged_class<F>(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        f: F,
    ) -> Option<smali::types::SmaliClass>
    where
        F: FnOnce(&mut smali::types::SmaliClass),
    {
        let bundle = self.bundle()?;
        let mut class = bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            })?;
        f(&mut class);
        Some(class)
    }

    /// Helper: stage a modified class and invalidate any open
    /// smali tabs viewing it.
    fn stage_smali_class_edit(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        modified: smali::types::SmaliClass,
        cx: &mut Context<Self>,
    ) {
        if let Some(bundle) = self.bundle_mut() {
            bundle.smali_edits.insert(crate::smali_edits::SmaliEdit {
                key: crate::smali_edits::SmaliEditKey {
                    artifact,
                    class_jni: class_jni.clone(),
                },
                modified,
            });
        }
        for tab in &mut self.tabs {
            if let TabKind::SmaliClass { class_jni: jni } = &tab.kind {
                if jni == &class_jni {
                    // Capture scroll position so we can restore the
                    // viewport after the line cache is rebuilt —
                    // otherwise every Enter on the op editor yanks
                    // the user back to the top of the file.
                    tab.pending_scroll_restore =
                        Some(tab.scroll.logical_scroll_top());
                    tab.lines = None;
                }
            }
        }
        cx.notify();
    }

    /// Remove a class-level annotation outright. Wired from the
    /// "× remove" affordance on the class-decl popover's annotation
    /// list.
    pub(crate) fn remove_class_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if index < class.annotations.len() {
                class.annotations.remove(index);
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    /// Remove a field annotation outright. Wired from the field
    /// popover's annotation list.
    pub(crate) fn remove_field_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        field_name: String,
        field_signature_jni: String,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if let Some(field) = class.fields.iter_mut().find(|f| {
                f.name == field_name && f.signature.to_jni() == field_signature_jni
            }) {
                if index < field.annotations.len() {
                    field.annotations.remove(index);
                }
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    /// Remove a method annotation outright. Wired from the method
    /// popover's annotation list.
    pub(crate) fn remove_method_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(modified) = self.with_staged_class(&artifact, &class_jni, |class| {
            if let Some(method) = class.methods.iter_mut().find(|m| {
                m.name == method_name
                    && m.signature.to_jni() == method_signature_jni
            }) {
                if index < method.annotations.len() {
                    method.annotations.remove(index);
                }
            }
        }) else {
            return;
        };
        self.stage_smali_class_edit(artifact, class_jni, modified, cx);
    }

    /// Annotations currently attached to `(artifact, class_jni)`,
    /// preferring the staged class when one exists. Returns
    /// (vis, type_jni) summaries suitable for the popover row
    /// list. Returns empty if the class isn't loaded.
    pub(crate) fn class_annotation_summaries(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
    ) -> Vec<(String, String)> {
        let Some(bundle) = self.bundle() else { return Vec::new() };
        let class = bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            });
        let Some(class) = class else { return Vec::new() };
        class
            .annotations
            .iter()
            .map(|a| (a.visibility.to_str().to_string(), a.annotation_type.to_jni()))
            .collect()
    }

    /// Same shape as `class_annotation_summaries`, scoped to a
    /// specific field within the class.
    pub(crate) fn field_annotation_summaries(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        field_name: &str,
        field_signature_jni: &str,
    ) -> Vec<(String, String)> {
        let Some(bundle) = self.bundle() else { return Vec::new() };
        let class = bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            });
        let Some(class) = class else { return Vec::new() };
        let Some(field) = class.fields.iter().find(|f| {
            f.name == field_name && f.signature.to_jni() == field_signature_jni
        }) else {
            return Vec::new();
        };
        field
            .annotations
            .iter()
            .map(|a| (a.visibility.to_str().to_string(), a.annotation_type.to_jni()))
            .collect()
    }

    /// Same shape as `field_annotation_summaries`, scoped to a
    /// method within the class.
    pub(crate) fn method_annotation_summaries(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        method_name: &str,
        method_signature_jni: &str,
    ) -> Vec<(String, String)> {
        let Some(bundle) = self.bundle() else { return Vec::new() };
        let class = bundle
            .smali_edits
            .get(artifact, class_jni)
            .map(|e| e.modified.clone())
            .or_else(|| {
                bundle
                    .smali_classes
                    .get(&(artifact.clone(), class_jni.to_string()))
                    .cloned()
            });
        let Some(class) = class else { return Vec::new() };
        let Some(method) = class.methods.iter().find(|m| {
            m.name == method_name && m.signature.to_jni() == method_signature_jni
        }) else {
            return Vec::new();
        };
        method
            .annotations
            .iter()
            .map(|a| (a.visibility.to_str().to_string(), a.annotation_type.to_jni()))
            .collect()
    }

    // ---- External editor ----------------------------------------------

    /// Stop the live-watch session. Drops the temp file. Doesn't
    /// touch any staged edits the watcher has already applied —
    /// those stay in the bundle and can be reverted from the
    /// Changes dialog like any other smali edit.
    pub(crate) fn stop_external_edit_watch(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.external_edit.as_mut() {
            // Signal the background poll task to exit. It'll see
            // the flag on its next tick (<=500ms) and stop. The
            // task itself drops the temp file when it exits — we
            // don't delete it here in case the next tick is mid-
            // way through a re-read.
            state.stop_requested = true;
        }
        cx.notify();
    }

    /// Entry point from the toolbar Edit File button. Writes the
    /// active smali class to a temp file, launches the OS's
    /// registered editor for `.smali` (without waiting), and
    /// starts a background poller that re-ingests the file on
    /// every save until the user clicks Stop on the toolbar chip.
    pub(crate) fn open_active_smali_in_external_editor(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.external_edit.is_some() {
            return;
        }
        let Some(active) = self.active_tab else { return };
        let Some(class_jni) = self.tabs.get(active).and_then(|t| match &t.kind {
            TabKind::SmaliClass { class_jni } => Some(class_jni.clone()),
            _ => None,
        }) else {
            return;
        };
        // Find the (artifact, current body). Prefer the staged
        // edit so the external editor sees what's in the GUI.
        let (artifact, body, class_display) = {
            let Some(bundle) = self.bundle() else { return };
            let owner = bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
                if jni == &class_jni {
                    Some((aid.clone(), c.clone()))
                } else {
                    None
                }
            });
            let Some((artifact, original)) = owner else { return };
            let display = original.name.as_java_type();
            let current = bundle
                .smali_edits
                .get(&artifact, &class_jni)
                .map(|e| e.modified.clone())
                .unwrap_or(original);
            (artifact, current.to_smali(), display)
        };
        let temp_path = match crate::external_editor::write_temp_file(&class_jni, &body)
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("external edit: write temp failed: {e}");
                return;
            }
        };
        if let Err(e) = crate::external_editor::launch_editor(&temp_path) {
            tracing::warn!("external edit: launch failed: {e}");
            // Stash the path on Shell so the chip can surface a
            // launch error even though the editor never opened —
            // user might want to inspect / open it manually.
            self.external_edit = Some(crate::external_editor::ExternalEditState {
                artifact,
                class_jni,
                class_display,
                temp_path,
                last_mtime: std::time::SystemTime::UNIX_EPOCH,
                last_error: Some(format!("launch failed: {e}")),
                stop_requested: false,
            });
            cx.notify();
            return;
        }
        let now = crate::external_editor::mtime(&temp_path);
        self.external_edit = Some(crate::external_editor::ExternalEditState {
            artifact: artifact.clone(),
            class_jni: class_jni.clone(),
            class_display,
            temp_path: temp_path.clone(),
            last_mtime: now,
            last_error: None,
            stop_requested: false,
        });
        cx.notify();
        self.spawn_external_edit_poll(artifact, class_jni, temp_path, cx);
    }

    // PollAction is the per-tick verdict the foreground hands
    // back to the polling task — kept private to this method.
    /// Background polling loop. Ticks at ~500ms; on every tick
    /// stats the temp file and, if mtime moved forward, re-reads
    /// and re-parses on the foreground thread. Exits when the
    /// session's `stop_requested` flag flips or the session
    /// disappears entirely (e.g. bundle closed).
    fn spawn_external_edit_poll(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        temp_path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                // Probe the file's mtime off the foreground so a
                // slow disk can't stall the UI. The result we
                // need to compare against lives on Shell, so we
                // hand it to the foreground via `update`.
                let observed = crate::external_editor::mtime(&temp_path);
                let action = this.update(cx, |shell, _cx| {
                    let Some(state) = shell.external_edit.as_ref() else {
                        return PollAction::Exit;
                    };
                    if state.artifact != artifact || state.class_jni != class_jni {
                        return PollAction::Exit;
                    }
                    if state.stop_requested {
                        return PollAction::StopAndCleanup;
                    }
                    if observed > state.last_mtime {
                        PollAction::Ingest(observed)
                    } else {
                        PollAction::Continue
                    }
                });
                let action = match action {
                    Ok(a) => a,
                    Err(_) => return, // entity gone
                };
                match action {
                    PollAction::Exit => return,
                    PollAction::Continue => continue,
                    PollAction::StopAndCleanup => {
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(state) = shell.external_edit.take() {
                                let _ = std::fs::remove_file(&state.temp_path);
                            }
                            cx.notify();
                        });
                        return;
                    }
                    PollAction::Ingest(new_mtime) => {
                        let body = match std::fs::read_to_string(&temp_path) {
                            Ok(s) => s,
                            Err(e) => {
                                let _ = this.update(cx, |shell, cx| {
                                    shell.record_external_edit_error(
                                        &artifact,
                                        &class_jni,
                                        format!("reading temp file: {e}"),
                                        new_mtime,
                                        cx,
                                    );
                                });
                                continue;
                            }
                        };
                        let _ = this.update(cx, |shell, cx| {
                            shell.ingest_external_edit(
                                &artifact,
                                &class_jni,
                                &body,
                                new_mtime,
                                cx,
                            );
                        });
                    }
                }
            }
        })
        .detach();
    }

    /// Foreground handler for a single observed save. Parses and
    /// stages on success, records the error on failure. Caller
    /// guarantees the session is still active and matches
    /// `(artifact, class_jni)`.
    fn ingest_external_edit(
        &mut self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        body: &str,
        observed_mtime: std::time::SystemTime,
        cx: &mut Context<Self>,
    ) {
        let parsed = match glass_api::parse_smali_class(body) {
            Ok(c) => c,
            Err(e) => {
                self.record_external_edit_error(
                    artifact,
                    class_jni,
                    format!("{e:#}"),
                    observed_mtime,
                    cx,
                );
                return;
            }
        };
        let body_jni = glass_api::smali_class_jni(&parsed);
        if body_jni != class_jni {
            self.record_external_edit_error(
                artifact,
                class_jni,
                format!(
                    "body declares class {body_jni:?} but this session edits {class_jni:?}"
                ),
                observed_mtime,
                cx,
            );
            return;
        }
        self.stage_smali_class_edit(artifact.clone(), class_jni.to_string(), parsed, cx);
        if let Some(state) = self.external_edit.as_mut() {
            state.last_mtime = observed_mtime;
            state.last_error = None;
        }
        cx.notify();
    }

    fn record_external_edit_error(
        &mut self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        msg: String,
        observed_mtime: std::time::SystemTime,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.external_edit.as_mut() {
            // Only update if the session is still the same — the
            // user might have stopped and restarted in the gap.
            if &state.artifact == artifact && state.class_jni == class_jni {
                state.last_error = Some(msg);
                state.last_mtime = observed_mtime;
            }
        }
        cx.notify();
    }

    // ---- Frida gadget injection ---------------------------------------

    /// Open the gadget-injection dialog for the currently-loaded
    /// bundle. Computes an `InjectionPlan` synchronously (it's
    /// pure inspection) and stashes the result on Shell so the
    /// dialog renderer doesn't have to rebuild it every frame.
    ///
    /// Returns `false` and no-ops when:
    ///   * No bundle is loaded.
    ///   * The bundle isn't an APK (no AndroidManifest).
    ///   * The bundle's manifest failed to decode.
    /// In each case we log a hint and leave the picker dropdown
    /// open so the user can see why nothing happened.
    pub(crate) fn open_injection_dialog(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(bundle) = self.bundle() else {
            tracing::info!("open_injection_dialog: no bundle loaded");
            return false;
        };
        let Some(manifest) = bundle.android_manifest.as_ref() else {
            tracing::info!(
                "open_injection_dialog: bundle has no AndroidManifest — \
                 gadget injection is APK-only for now"
            );
            return false;
        };
        // Collect inputs for the planner. `loaded_classes` is
        // the set of JNI sigs we've lifted smali for; the
        // planner uses it to warn when the manifest references
        // a class we don't actually have.
        let loaded_classes: std::collections::BTreeSet<String> = bundle
            .smali_classes
            .keys()
            .map(|(_, jni)| jni.clone())
            .collect();
        // ABIs the APK carries native libs for — `lib/<abi>/`.
        // We read this from the existing `origins` field where
        // each native-lib leaf records its `lib/<abi>/<name>`
        // string.
        let mut native_abis: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        let mut abis_with_gadget: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for (i, origin) in bundle.origins.iter().enumerate() {
            // origins like "lib/arm64-v8a" (set by the loader
            // for native-lib leaves). Strip the prefix to get
            // the ABI string.
            let s = origin.as_ref();
            if let Some(abi) = s.strip_prefix("lib/") {
                native_abis.insert(abi.to_string());
                // If a leaf labelled libfrida-gadget.so sits in
                // this ABI directory, flag the warning. The
                // leaf's label is the bare filename.
                if let Some(label) = bundle.labels.get(i) {
                    if label.as_ref() == "libfrida-gadget.so" {
                        abis_with_gadget.insert(abi.to_string());
                    }
                }
            }
        }
        let inputs = glass_frida::PlanInputs {
            manifest: Some(&**manifest),
            loaded_classes,
            native_abis,
            abis_with_gadget,
        };
        let plan = match glass_frida::plan_injection(&inputs) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(?e, "open_injection_dialog: planner refused");
                return false;
            }
        };
        // Capture which device the user has currently selected
        // so the dialog can offer "Inject & install on …" even
        // if the chip selection changes while the dialog is
        // open.
        let target_device = self
            .selected_device
            .as_ref()
            .and_then(|id| {
                self.device_snapshot.iter().find(|d| &d.id == id).cloned()
            });
        self.injection_dialog = Some(crate::InjectionDialogState {
            plan,
            target_device,
        });
        // Close the picker dropdown so the dialog is the only
        // overlay competing for attention.
        self.device_picker_open = false;
        cx.notify();
        true
    }

    pub(crate) fn close_injection_dialog(&mut self, cx: &mut Context<Self>) {
        if self.injection_dialog.take().is_some() {
            cx.notify();
        }
    }

    /// Apply the gadget-injection plan to the loaded bundle.
    /// Stages a smali edit on the patch-target class (visible
    /// in the Changes dialog like any other smali edit) and
    /// registers `lib/<abi>/libfrida-gadget.so` as a pending
    /// APK addition for every supported ABI in the plan.
    ///
    /// After this the user clicks the toolbar's existing
    /// "Export N changes…" button to write the patched APK.
    /// Sign + install are M3.2d/e.
    pub(crate) fn execute_injection(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.injection_dialog.as_ref() else { return };
        // Pluck the JNI of the patch-target class so we can
        // locate it in the bundle.
        let target_jni = match &state.plan.patch_target {
            glass_frida::PatchTarget::ExistingApplication { class_jni, .. } => {
                class_jni.clone()
            }
            glass_frida::PatchTarget::LauncherActivity { class_jni, .. } => {
                class_jni.clone()
            }
            glass_frida::PatchTarget::SynthesiseRequired => {
                tracing::warn!(
                    "execute_injection: plan needs class synthesis (not implemented)"
                );
                self.close_injection_dialog(cx);
                return;
            }
        };
        let plan = state.plan.clone();
        // Find the artifact (DEX) that contains this class.
        // smali_classes is keyed by (artifact_id, class_jni)
        // so we can lift the class out and learn its DEX in
        // one pass.
        let (artifact_id, base_class) = {
            let Some(bundle) = self.bundle() else {
                self.close_injection_dialog(cx);
                return;
            };
            let hit = bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
                if jni == &target_jni {
                    Some((aid.clone(), c.clone()))
                } else {
                    None
                }
            });
            match hit {
                Some(x) => x,
                None => {
                    tracing::warn!(
                        target_jni = %target_jni,
                        "execute_injection: class isn't in the lifted set"
                    );
                    self.close_injection_dialog(cx);
                    return;
                }
            }
        };
        // Layer on top of any earlier staged edit for the same
        // class so the gadget patch coexists with whatever the
        // user might have changed manually.
        let starting_class = self
            .bundle()
            .and_then(|b| b.smali_edits.get(&artifact_id, &target_jni))
            .map(|e| e.modified.clone())
            .unwrap_or(base_class);
        let patched = match glass_frida::apply_plan(&starting_class, &plan) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(?e, "execute_injection: apply_plan failed");
                self.close_injection_dialog(cx);
                return;
            }
        };
        // Stage the modified class through the existing path
        // so it shows up in the Changes dialog like any other
        // smali edit (revertable, line-cached invalidation,
        // tinting on the smali tab).
        self.stage_smali_class_edit(artifact_id, target_jni, patched, cx);
        // Add the gadget binary to the bundle's pending APK
        // additions for every ABI we ship a gadget for.
        // Today that's arm64-v8a only; other ABIs in the plan
        // are skipped with a log line so the user can see what
        // didn't make it.
        let mut added_abis: Vec<String> = Vec::new();
        let mut skipped_abis: Vec<String> = Vec::new();
        // Every gadget binary needs its config sibling — recent
        // gadget releases (17.x) refuse to load without
        // libfrida-gadget.config.so next to them. Stage the
        // listen-mode config alongside every .so we add.
        let config_filename = glass_frida::ANDROID_GADGET_CONFIG_FILENAME;
        let config_bytes = glass_frida::android_gadget_config_listen();
        for abi in &plan.abis {
            match glass_frida::for_android_abi(abi) {
                Some(gadget) => {
                    if let Some(bundle) = self.bundle_mut() {
                        let zip_path = format!("lib/{abi}/{}", gadget.filename);
                        bundle
                            .pending_additions
                            .insert(zip_path, gadget.bytes.to_vec());
                        let cfg_path = format!("lib/{abi}/{config_filename}");
                        bundle
                            .pending_additions
                            .insert(cfg_path, config_bytes.clone());
                    }
                    added_abis.push(abi.clone());
                }
                None => skipped_abis.push(abi.clone()),
            }
        }
        // If no ABI matched, also drop the gadget under
        // arm64-v8a regardless — Android will pick it up on
        // arm64 phones even if the APK didn't ship any other
        // arm64 libs. (Devices choose libs by ABI; an APK with
        // only x86 libs but with arm64-v8a frida-gadget will
        // load it correctly on a Pixel.)
        if added_abis.is_empty() {
            if let Some(gadget) = glass_frida::for_android_abi("arm64-v8a") {
                if let Some(bundle) = self.bundle_mut() {
                    bundle.pending_additions.insert(
                        format!("lib/arm64-v8a/{}", gadget.filename),
                        gadget.bytes.to_vec(),
                    );
                    bundle.pending_additions.insert(
                        format!("lib/arm64-v8a/{config_filename}"),
                        config_bytes.clone(),
                    );
                }
                added_abis.push("arm64-v8a".to_string());
            }
        }
        tracing::info!(
            added = ?added_abis,
            skipped = ?skipped_abis,
            "gadget bytes registered as pending APK additions",
        );
        self.close_injection_dialog(cx);
    }

    /// Full "Inject & Install" pipeline. Stages the smali edit
    /// + gadget addition (same as `execute_injection`), then on
    /// a background task: writes a temp APK via the existing
    /// export pipeline, signs it with the Glass debug keystore,
    /// and `adb install -r`s it on the target device. Progress
    /// is reported on `Shell.injection_progress` so the GUI
    /// can show a streaming status overlay.
    pub(crate) fn execute_injection_and_install(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        // First stage the changes via the existing path so the
        // Changes dialog still shows what got patched.
        let Some(state) = self.injection_dialog.clone() else { return };
        let Some(target) = state.target_device.clone() else {
            tracing::warn!("execute_injection_and_install: no device selected");
            return;
        };
        if !matches!(target.state, glass_device::AuthState::Authorised) {
            self.injection_progress = Some(crate::InjectionProgress {
                phase: crate::InjectionPhase::Done,
                log: vec![format!(
                    "Device {} isn't authorised — accept the USB-debug prompt on it first.",
                    target.id.serial,
                )],
                result: Some(Err("device unauthorised".into())),
            });
            self.close_injection_dialog(cx);
            cx.notify();
            return;
        }
        if !matches!(target.id.platform, glass_device::DevicePlatform::Android) {
            self.injection_progress = Some(crate::InjectionProgress {
                phase: crate::InjectionPhase::Done,
                log: vec![format!(
                    "Selected device {} is iOS — inject-and-install is Android-only for now.",
                    target.id.serial,
                )],
                result: Some(Err("ios install path not implemented".into())),
            });
            self.close_injection_dialog(cx);
            cx.notify();
            return;
        }
        // Discover sign tools *before* any disk writes. If
        // they're missing the user sees a clean error rather
        // than a half-baked patched APK on disk.
        let signer = match glass_frida::SignerTools::discover() {
            Ok(s) => s,
            Err(e) => {
                self.injection_progress = Some(crate::InjectionProgress {
                    phase: crate::InjectionPhase::Done,
                    log: vec![format!("{e}")],
                    result: Some(Err(format!("sign tools missing: {e}"))),
                });
                self.close_injection_dialog(cx);
                cx.notify();
                return;
            }
        };
        // Reuse `execute_injection` to stage the smali edit +
        // gadget addition. This closes the dialog as a
        // side-effect (it always does); we don't need to call
        // close again.
        self.execute_injection(cx);
        // Build the inputs for the background task.
        let source_path = self
            .source_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let stem = source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("patched");
        let temp_dir = std::env::temp_dir().join("glass-inject");
        if let Err(e) = std::fs::create_dir_all(&temp_dir) {
            self.injection_progress = Some(crate::InjectionProgress {
                phase: crate::InjectionPhase::Done,
                log: vec![format!("creating {}: {e}", temp_dir.display())],
                result: Some(Err(format!("tempdir: {e}"))),
            });
            cx.notify();
            return;
        }
        let out_path = temp_dir.join(format!("{stem}-frida.apk"));
        // Snapshot everything the executor needs so the
        // background task doesn't hold a borrow on Shell.
        let Some(bundle) = self.bundle() else { return };
        let mut edit_map: std::collections::HashMap<
            glass_db::ArtifactId,
            Vec<glass_api::EditPatch>,
        > = std::collections::HashMap::new();
        for e in bundle.edits.entries() {
            edit_map
                .entry(e.artifact.clone())
                .or_default()
                .push(glass_api::EditPatch {
                    vaddr: e.vaddr,
                    new_bytes: e.new_bytes.clone(),
                });
        }
        let mut smali_edit_map: glass_api::SmaliEditMap =
            std::collections::HashMap::new();
        for e in bundle.smali_edits.entries() {
            smali_edit_map
                .entry(e.key.artifact.clone())
                .or_default()
                .insert(e.key.class_jni.clone(), e.modified.clone());
        }
        let additions: glass_api::ApkAdditions = bundle.pending_additions.clone();
        let serial = target.id.serial.clone();
        let device_manager = self.device_manager.clone();
        // Initial progress state.
        self.injection_progress = Some(crate::InjectionProgress {
            phase: crate::InjectionPhase::Exporting,
            log: vec![format!("Writing patched APK to {}", out_path.display())],
            result: None,
        });
        cx.notify();
        // Spawn the pipeline.
        cx.spawn(async move |this, cx| {
            // Phase 1: export.
            let export_result: Result<(), String> = cx
                .background_executor()
                .spawn({
                    let source_path = source_path.clone();
                    let out_path = out_path.clone();
                    async move {
                        match glass_api::open(&source_path) {
                            Ok(bundle) => glass_api::export_to_path_with_smali(
                                &bundle,
                                &edit_map,
                                &smali_edit_map,
                                &additions,
                                &out_path,
                            )
                            .map_err(|e| format!("{e:#}")),
                            Err(e) => Err(format!("re-open failed: {e:#}")),
                        }
                    }
                })
                .await;
            if let Err(e) = export_result {
                let _ = this.update(cx, |shell, cx| {
                    let log = vec![format!("Export failed: {e}")];
                    shell.injection_progress = Some(crate::InjectionProgress {
                        phase: crate::InjectionPhase::Done,
                        log,
                        result: Some(Err(e)),
                    });
                    cx.notify();
                });
                return;
            }
            // Phase 2: sign.
            let _ = this.update(cx, |shell, cx| {
                if let Some(p) = shell.injection_progress.as_mut() {
                    p.phase = crate::InjectionPhase::Signing;
                    p.log.push(format!(
                        "Signing with {}",
                        signer.keystore_path.display()
                    ));
                }
                cx.notify();
            });
            let signer_for_task = signer.clone();
            let out_path_for_task = out_path.clone();
            let sign_result: Result<String, glass_frida::SignError> = cx
                .background_executor()
                .spawn(async move {
                    signer_for_task.ensure_keystore()?;
                    signer_for_task.sign(&out_path_for_task)
                })
                .await;
            match sign_result {
                Ok(stdout) => {
                    let _ = this.update(cx, |shell, cx| {
                        if let Some(p) = shell.injection_progress.as_mut() {
                            if !stdout.trim().is_empty() {
                                p.log.push(stdout.trim().to_string());
                            }
                        }
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |shell, cx| {
                        shell.injection_progress = Some(crate::InjectionProgress {
                            phase: crate::InjectionPhase::Done,
                            log: vec![format!("Sign failed: {e}")],
                            result: Some(Err(format!("{e}"))),
                        });
                        cx.notify();
                    });
                    return;
                }
            }
            // Phase 3: adb install.
            let _ = this.update(cx, |shell, cx| {
                if let Some(p) = shell.injection_progress.as_mut() {
                    p.phase = crate::InjectionPhase::Installing;
                    p.log.push(format!("adb -s {serial} install -r"));
                }
                cx.notify();
            });
            let serial_for_task = serial.clone();
            let out_for_task = out_path.clone();
            let install_result: Result<String, glass_device::DeviceError> = cx
                .background_executor()
                .spawn(async move {
                    let status = device_manager.backend_status();
                    let adb = status
                        .adb
                        .map_err(|e| glass_device::DeviceError::Backend(format!("adb: {e}")))?;
                    // We need a fresh AdbBackend here — backend_status
                    // returned info, but the install verb lives on
                    // the backend itself. Re-discover the binary.
                    let backend = glass_device::adb::AdbBackend::with_override(
                        Some(adb.binary_path),
                    )
                    .map_err(|e| glass_device::DeviceError::Backend(format!("{e}")))?;
                    backend.install(&serial_for_task, &out_for_task)
                })
                .await;
            let _ = this.update(cx, |shell, cx| {
                let mut p = shell
                    .injection_progress
                    .take()
                    .unwrap_or_else(|| crate::InjectionProgress {
                        phase: crate::InjectionPhase::Done,
                        log: Vec::new(),
                        result: None,
                    });
                p.phase = crate::InjectionPhase::Done;
                match install_result {
                    Ok(stdout) => {
                        if !stdout.trim().is_empty() {
                            p.log.push(stdout.trim().to_string());
                        }
                        p.result = Some(Ok(out_path.clone()));
                    }
                    Err(e) => {
                        p.log.push(format!("Install failed: {e}"));
                        p.result = Some(Err(format!("{e}")));
                    }
                }
                shell.injection_progress = Some(p);
                // We just changed the device's state — the
                // newly-installed app probably hasn't launched
                // yet so the chip should re-probe and reflect
                // current reality, not the cached "yes Frida"
                // from before we ran.
                if let Some(id) = shell.selected_device.as_ref() {
                    shell.frida_probes.remove(id);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Copy the current dock log to the system clipboard.
    /// Workaround for gpui's lack of native text selection
    /// in the dock — the user can now grab full error
    /// messages instead of squinting.
    pub(crate) fn copy_debug_dock_log(&mut self, cx: &mut Context<Self>) {
        let Some(dock) = self.debug_dock.as_ref() else { return };
        let joined = dock.log.join("\n");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(joined));
        // Tiny confirmation so the user knows it landed —
        // appending to the dock log instead of a toast keeps
        // the noise local.
        self.push_dock_log("(log copied to clipboard)", cx);
    }

    pub(crate) fn dismiss_injection_progress(&mut self, cx: &mut Context<Self>) {
        if self.injection_progress.take().is_some() {
            cx.notify();
        }
    }

    // ---- Debug dock ----------------------------------------------------

    /// Open the bottom debug dock against the currently-selected
    /// device + loaded APK. Captures the device snapshot + the
    /// bundle's package name + the latest probe's agent version
    /// at connect time so the dock stays anchored even if the
    /// chip selection changes underneath.
    pub(crate) fn open_debug_dock(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.selected_device.clone() else { return false };
        let Some(device) = self
            .device_snapshot
            .iter()
            .find(|d| d.id == id)
            .cloned()
        else {
            return false;
        };
        let Some(bundle) = self.bundle() else { return false };
        let Some(manifest) = bundle.android_manifest.as_ref() else {
            return false;
        };
        let Some(package) = manifest.package_name().map(|s| s.to_string())
        else {
            return false;
        };
        let agent_version = self
            .frida_probes
            .get(&id)
            .and_then(|c| c.result.as_ref().ok())
            .and_then(|r| r.agent_version.clone());
        // Spawn the Frida session actor up front. We hand it
        // the dock immediately so the UI can render; the
        // actual attach runs on a background task and
        // populates `session` when it completes.
        let session = glass_frida::Session::spawn();
        self.debug_dock = Some(crate::DebugDockState {
            device: device.clone(),
            package: package.clone(),
            agent_version,
            log: vec![format!("connecting to {package}…")],
            height: gpui::px(180.),
            session: Some(session.clone()),
            attaching: true,
        });
        // The dock comes from the picker dropdown — close that
        // so the chip doesn't fight the new dock for attention.
        self.device_picker_open = false;
        cx.notify();
        // Kick off the attach. Two steps off the foreground
        // executor:
        //   1. Resolve the package's PID via `adb shell pidof`.
        //   2. Set up `adb forward tcp:27442 tcp:27042` (the
        //      gadget probe already does this, harmless to
        //      repeat — adb just returns the same port).
        //   3. Ask Frida to add a remote device at the
        //      forwarded address + attach to that PID.
        let device_manager = self.device_manager.clone();
        let serial = device.id.serial.clone();
        let package_for_task = package.clone();
        cx.spawn(async move |this, cx| {
            // Capture the resolved PID alongside the attach
            // outcome so the auto-resume step below can call
            // session.resume(pid).
            let attach_outcome: Result<(glass_frida::AttachReport, u32), String> = cx
                .background_executor()
                .spawn({
                    let session = session.clone();
                    async move {
                        let status = device_manager.backend_status().adb.clone();
                        let Ok(adb_info) = status else {
                            return Err("ADB not available".to_string());
                        };
                        let backend = glass_device::adb::AdbBackend::with_override(
                            Some(adb_info.binary_path.clone()),
                        )
                        .map_err(|e| format!("adb backend: {e}"))?;
                        let pid_out = backend
                            .shell(&serial, &["pidof", &package_for_task])
                            .map_err(|e| format!("pidof: {e}"))?;
                        let pid: u32 = pid_out
                            .split_whitespace()
                            .next()
                            .and_then(|s| s.parse().ok())
                            .ok_or_else(|| {
                                format!("{package_for_task} isn't running on the device — launch it first")
                            })?;
                        let _ = backend.probe_gadget(&serial);
                        let rep = session
                            .attach_remote("127.0.0.1:27442", pid)
                            .map_err(|e| format!("attach {pid}: {e}"))?;
                        Ok((rep, pid))
                    }
                })
                .await;
            // First update: surface the attach result and
            // decide whether we should auto-resume. Empty
            // registries → resume immediately so the user
            // can use the app. Non-empty → leave paused so
            // they can install / verify hooks before letting
            // the app run.
            let (should_resume, pid_to_resume, session_for_resume) =
                this.update(cx, |shell, cx| {
                    // Read registry state up front so we don't
                    // hold a Shell-immutable + dock-mutable
                    // borrow at the same time.
                    let registries_empty = shell
                        .bundle()
                        .map(|b| b.traces.is_empty() && b.hooks.is_empty())
                        .unwrap_or(true);
                    if let Some(dock) = shell.debug_dock.as_mut() {
                        dock.attaching = false;
                        match &attach_outcome {
                            Ok((_, pid)) => {
                                dock.log.push("connected".to_string());
                                let sess = dock.session.clone();
                                cx.notify();
                                (registries_empty, Some(*pid), sess)
                            }
                            Err(e) => {
                                dock.log.push(format!("attach failed: {e}"));
                                if let Some(s) = dock.session.take() {
                                    s.shutdown();
                                }
                                cx.notify();
                                (false, None, None)
                            }
                        }
                    } else {
                        (false, None, None)
                    }
                })
                .unwrap_or((false, None, None));
            if should_resume {
                if let (Some(pid), Some(session)) =
                    (pid_to_resume, session_for_resume)
                {
                    let resume_res = cx
                        .background_executor()
                        .spawn(async move { session.resume(pid) })
                        .await;
                    let _ = this.update(cx, |shell, cx| match resume_res {
                        Ok(()) => {
                            shell.push_dock_log(
                                "▶ auto-resumed (no traces / hooks defined)",
                                cx,
                            );
                        }
                        Err(e) => {
                            shell.push_dock_log(format!("resume failed: {e}"), cx);
                        }
                    });
                }
            } else if pid_to_resume.is_some() {
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(
                        "⏸ paused — click Restart after installing traces / hooks",
                        cx,
                    );
                });
            }
        })
        .detach();
        true
    }

    pub(crate) fn close_debug_dock(&mut self, cx: &mut Context<Self>) {
        if let Some(mut dock) = self.debug_dock.take() {
            if let Some(session) = dock.session.take() {
                // Best-effort detach, then drop everything.
                let _ = session.detach();
                session.shutdown();
            }
            cx.notify();
        }
    }

    /// Stash the current pointer Y and current dock height on
    /// mouse-down so subsequent mouse-moves can resize relative
    /// to a stable anchor instead of accumulating tiny deltas.
    pub(crate) fn start_dock_resize(
        &mut self,
        pointer_y: gpui::Pixels,
        _cx: &mut Context<Self>,
    ) {
        if let Some(dock) = self.debug_dock.as_ref() {
            self.debug_dock_resize_anchor = Some((pointer_y, dock.height));
        }
    }

    /// Apply a drag delta. The pointer moving *up* (smaller Y)
    /// grows the dock; moving down shrinks it.
    pub(crate) fn update_dock_resize(
        &mut self,
        pointer_y: gpui::Pixels,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.debug_dock_resize_anchor
        else {
            return;
        };
        let dy = anchor_y.as_f32() - pointer_y.as_f32();
        let new_h = gpui::px(anchor_h.as_f32() + dy);
        self.set_debug_dock_height(new_h, cx);
    }

    pub(crate) fn finish_dock_resize(&mut self, cx: &mut Context<Self>) {
        if self.debug_dock_resize_anchor.take().is_some() {
            cx.notify();
        }
    }

    /// Set the dock's height. Used by the drag-handle on the
    /// top edge; values are clamped to a sane range.
    pub(crate) fn set_debug_dock_height(
        &mut self,
        h: gpui::Pixels,
        cx: &mut Context<Self>,
    ) {
        if let Some(dock) = self.debug_dock.as_mut() {
            // Lower bound: enough to show the controls + a
            // single log line. Upper bound: half the window
            // (we don't know the window height here, so cap
            // at 800 — windows are typically taller than that
            // and the dock can be re-resized).
            let clamped = h.as_f32().clamp(80.0, 800.0);
            dock.height = gpui::px(clamped);
            cx.notify();
        }
    }

    /// Append a log line to the dock. Trims trailing whitespace
    /// and skips empty lines so the column stays tight.
    fn push_dock_log(&mut self, line: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(dock) = self.debug_dock.as_mut() {
            let line = line.into();
            for s in line.lines() {
                let trimmed = s.trim_end();
                if !trimmed.is_empty() {
                    dock.log.push(trimmed.to_string());
                }
            }
            // Keep the log bounded so a chatty action doesn't
            // OOM the dock. 200 lines = several screens of
            // history, plenty for the play/stop cadence.
            const MAX_LOG: usize = 200;
            if dock.log.len() > MAX_LOG {
                let drop = dock.log.len() - MAX_LOG;
                dock.log.drain(..drop);
            }
            cx.notify();
        }
    }

    /// Launch the dock's package on the dock's device. Runs
    /// `adb shell monkey -p <pkg> -c LAUNCHER 1` off the
    /// foreground; pipes the combined stdout/stderr into the
    /// dock's log column.
    /// Restart-with-hooks orchestrator. One click runs the
    /// whole "give me a fresh app instance with all my
    /// instrumentation in place" workflow:
    ///
    ///   1. `adb shell am force-stop <pkg>`.
    ///   2. `adb shell monkey -p <pkg> -c LAUNCHER 1`.
    ///   3. Poll the gadget port until it answers (gadget
    ///      is in `on_load: wait` so it'll be paused
    ///      inside <clinit>).
    ///   4. Resolve the new PID via `pidof <pkg>`.
    ///   5. Drop the old Frida session and attach to the
    ///      new PID via the same actor.
    ///   6. Re-render every trace + hook script (their
    ///      old script ids point at the dead session, so
    ///      we invalidate them) and load them against the
    ///      paused process.
    ///   7. Call `session.resume(pid)` — gadget unblocks,
    ///      app continues with hooks in place.
    ///
    /// When there are no traces / hooks defined, steps 6
    /// collapses to a no-op so this is also the correct
    /// "just restart the app" button.
    pub(crate) fn debug_restart(&mut self, cx: &mut Context<Self>) {
        let Some(dock) = self.debug_dock.as_ref() else { return };
        let serial = dock.device.id.serial.clone();
        let package = dock.package.clone();
        let device_manager = self.device_manager.clone();
        // Snapshot the trace + hook registries so we can
        // re-install after the new attach. Cloning is cheap
        // (each entry is a few strings + bounded Vec).
        let traces: Vec<crate::traces::TraceEntry> = self
            .bundle()
            .map(|b| b.traces.entries().iter().map(|&e| e.clone()).collect())
            .unwrap_or_default();
        let hooks: Vec<crate::hooks::HookEntry> = self
            .bundle()
            .map(|b| b.hooks.entries().iter().map(|&e| e.clone()).collect())
            .unwrap_or_default();
        // Drop the old session — the process we're about to
        // kill owns it.
        let old_session = self
            .debug_dock
            .as_mut()
            .and_then(|d| d.session.take());
        self.push_dock_log(format!("↻ restarting {package}"), cx);
        cx.spawn(async move |this, cx| {
            // Tear down old session off the main thread.
            if let Some(s) = old_session {
                let _ = cx
                    .background_executor()
                    .spawn(async move {
                        let _ = s.detach();
                        s.shutdown();
                    })
                    .await;
            }
            // ADB backend handle — reused for every step.
            let adb = match cx
                .background_executor()
                .spawn({
                    let dm = device_manager.clone();
                    async move {
                        let status = dm.backend_status().adb.clone()
                            .map_err(|e| format!("adb: {e}"))?;
                        glass_device::adb::AdbBackend::with_override(
                            Some(status.binary_path),
                        )
                        .map_err(|e| format!("adb backend: {e}"))
                    }
                })
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    let _ = this.update(cx, |shell, cx| {
                        shell.push_dock_log(format!("✗ {e}"), cx);
                    });
                    return;
                }
            };
            // 1. force-stop.
            let _ = cx
                .background_executor()
                .spawn({
                    let serial = serial.clone();
                    let package = package.clone();
                    let adb = adb.clone();
                    async move { adb.force_stop(&serial, &package) }
                })
                .await;
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log("• stopped", cx);
            });
            // 2. start.
            let start_res = cx
                .background_executor()
                .spawn({
                    let serial = serial.clone();
                    let package = package.clone();
                    let adb = adb.clone();
                    async move { adb.start_main_activity(&serial, &package) }
                })
                .await;
            if let Err(e) = start_res {
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(format!("✗ start: {e}"), cx);
                });
                return;
            }
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log("• launched", cx);
            });
            // 3. wait for the gadget to come back up. Gadget
            //    is in on_load:wait so it'll bind 27042 inside
            //    <clinit> and block; probe_gadget returns
            //    Ok(true) once that happens. Poll for up to
            //    ~10s; clinit usually fires within 1-2s.
            let gadget_alive = cx
                .background_executor()
                .spawn({
                    let serial = serial.clone();
                    let adb = adb.clone();
                    async move {
                        for _ in 0..50 {
                            if let Ok(true) = adb.probe_gadget(&serial) {
                                return true;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        false
                    }
                })
                .await;
            if !gadget_alive {
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(
                        "✗ gadget never came back up (10s timeout)",
                        cx,
                    );
                });
                return;
            }
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log("• gadget ready", cx);
            });
            // 4. resolve PID.
            let pid_str = cx
                .background_executor()
                .spawn({
                    let serial = serial.clone();
                    let package = package.clone();
                    let adb = adb.clone();
                    async move { adb.shell(&serial, &["pidof", &package]) }
                })
                .await;
            let pid: u32 = match pid_str {
                Ok(s) => match s.split_whitespace().next().and_then(|t| t.parse().ok()) {
                    Some(n) => n,
                    None => {
                        let _ = this.update(cx, |shell, cx| {
                            shell.push_dock_log("✗ couldn't parse pid", cx);
                        });
                        return;
                    }
                },
                Err(e) => {
                    let _ = this.update(cx, |shell, cx| {
                        shell.push_dock_log(format!("✗ pidof: {e}"), cx);
                    });
                    return;
                }
            };
            // 5. fresh actor + attach.
            let session = glass_frida::Session::spawn();
            let attach_res = cx
                .background_executor()
                .spawn({
                    let session = session.clone();
                    async move { session.attach_remote("127.0.0.1:27442", pid) }
                })
                .await;
            if let Err(e) = attach_res {
                session.shutdown();
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(format!("✗ attach pid {pid}: {e}"), cx);
                });
                return;
            }
            let _ = this.update(cx, |shell, cx| {
                if let Some(dock) = shell.debug_dock.as_mut() {
                    dock.session = Some(session.clone());
                    dock.attaching = false;
                }
                shell.push_dock_log(format!("• attached pid {pid}", pid = pid), cx);
                cx.notify();
            });
            // 6. re-install every trace + hook script.
            //    We render fresh JS each time because the
            //    old script ids belong to the dead session.
            //    Errors here don't block resume — we'd
            //    rather get the app running with a partial
            //    set than freeze waiting for one bad trace.
            let mut installed = 0usize;
            let mut failed = 0usize;
            for entry in &traces {
                let new_id = session.alloc_script_id();
                let js = match glass_frida::render_trace_script(
                    &entry.key.class_jni,
                    &entry.key.method_name,
                    &entry.key.method_signature,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        failed += 1;
                        let key = entry.key.clone();
                        let err = format!("{e}");
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.traces.mark_failed(&key, err.clone());
                            }
                            shell.push_dock_log(
                                format!("✗ trace render {}.{}: {err}",
                                    key.class_jni, key.method_name),
                                cx,
                            );
                        });
                        continue;
                    }
                };
                let name = format!(
                    "trace-{}-{}",
                    entry.key.class_jni.replace('/', "."),
                    entry.key.method_name
                );
                let key = entry.key.clone();
                let res = cx
                    .background_executor()
                    .spawn({
                        let session = session.clone();
                        async move { session.create_script(new_id, name, js) }
                    })
                    .await;
                match res {
                    Ok(()) => {
                        installed += 1;
                        let _ = this.update(cx, |shell, _cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                // Clear stale by_script entry
                                // before remapping the key to
                                // the new ScriptId.
                                if let Some(existing) = bundle.traces.get(&key) {
                                    let _ = existing;
                                }
                                // mark_active rewrites by_script
                                // for new_id; we also need to
                                // wipe invocations from the
                                // previous run so the dialog
                                // hit-count resets.
                                bundle.traces.mark_active(&key, new_id);
                                if let Some(e) = bundle.traces.get_mut(&key) {
                                    e.invocations.clear();
                                }
                            }
                        });
                    }
                    Err(e) => {
                        failed += 1;
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.traces.mark_failed(&key, e.clone());
                            }
                            shell.push_dock_log(
                                format!("✗ trace {}.{}: {e}",
                                    key.class_jni, key.method_name),
                                cx,
                            );
                        });
                    }
                }
            }
            for entry in &hooks {
                let new_id = session.alloc_script_id();
                let body = match &entry.action {
                    crate::hooks::HookAction::LogOnly => {
                        glass_frida::HookBody::LogOnly
                    }
                    crate::hooks::HookAction::ReturnLiteral(lit) => {
                        glass_frida::HookBody::ReturnLiteral(lit.clone())
                    }
                    crate::hooks::HookAction::CustomJs(body) => {
                        glass_frida::HookBody::Custom(body.clone())
                    }
                };
                let js = match glass_frida::render_hook_script(
                    &entry.key.class_jni,
                    &entry.key.method_name,
                    &entry.key.method_signature,
                    &body,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        failed += 1;
                        let key = entry.key.clone();
                        let err = format!("{e}");
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.hooks.mark_failed(&key, err.clone());
                            }
                            shell.push_dock_log(
                                format!("✗ hook render {}.{}: {err}",
                                    key.class_jni, key.method_name),
                                cx,
                            );
                        });
                        continue;
                    }
                };
                let name = format!(
                    "hook-{}-{}",
                    entry.key.class_jni.replace('/', "."),
                    entry.key.method_name
                );
                let key = entry.key.clone();
                let res = cx
                    .background_executor()
                    .spawn({
                        let session = session.clone();
                        async move { session.create_script(new_id, name, js) }
                    })
                    .await;
                match res {
                    Ok(()) => {
                        installed += 1;
                        let _ = this.update(cx, |shell, _cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.hooks.mark_active(&key, new_id);
                                if let Some(e) = bundle.hooks.get_mut(&key) {
                                    e.invocations.clear();
                                }
                            }
                        });
                    }
                    Err(e) => {
                        failed += 1;
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(bundle) = shell.bundle_mut() {
                                bundle.hooks.mark_failed(&key, e.clone());
                            }
                            shell.push_dock_log(
                                format!("✗ hook {}.{}: {e}",
                                    key.class_jni, key.method_name),
                                cx,
                            );
                        });
                    }
                }
            }
            let total = traces.len() + hooks.len();
            if total > 0 {
                let _ = this.update(cx, |shell, cx| {
                    shell.push_dock_log(
                        format!("• installed {installed}/{total} ({failed} failed)"),
                        cx,
                    );
                });
            }
            // 7. resume — gadget unblocks, app starts running.
            let resume_res = cx
                .background_executor()
                .spawn({
                    let session = session.clone();
                    async move { session.resume(pid) }
                })
                .await;
            let _ = this.update(cx, |shell, cx| match resume_res {
                Ok(()) => {
                    shell.push_dock_log("▶ resumed — app running", cx);
                }
                Err(e) => {
                    shell.push_dock_log(format!("✗ resume: {e}"), cx);
                }
            });
        })
        .detach();
    }

    /// Force-stop the dock's package on the dock's device.
    pub(crate) fn debug_stop(&mut self, cx: &mut Context<Self>) {
        let Some(dock) = self.debug_dock.as_ref() else { return };
        let serial = dock.device.id.serial.clone();
        let package = dock.package.clone();
        let device_manager = self.device_manager.clone();
        self.push_dock_log(format!("◼ stopping {package}"), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let status = device_manager.backend_status().adb.clone();
                    let backend = match status {
                        Ok(info) => {
                            glass_device::adb::AdbBackend::with_override(
                                Some(info.binary_path),
                            )
                        }
                        Err(e) => Err(glass_device::DeviceError::Backend(
                            format!("{e}"),
                        )),
                    };
                    match backend {
                        Ok(b) => b.force_stop(&serial, &package),
                        Err(e) => Err(e),
                    }
                })
                .await;
            let line = match result {
                Ok(s) if s.trim().is_empty() => "(stopped)".to_string(),
                Ok(s) => s,
                Err(e) => format!("error: {e}"),
            };
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log(line, cx);
            });
        })
        .detach();
    }

    pub(crate) fn toggle_traces_dialog(&mut self, cx: &mut Context<Self>) {
        self.traces_dialog_open = !self.traces_dialog_open;
        cx.notify();
    }

    pub(crate) fn close_traces_dialog(&mut self, cx: &mut Context<Self>) {
        if self.traces_dialog_open {
            self.traces_dialog_open = false;
            cx.notify();
        }
    }

    /// Stop every active trace. Used by the "Stop all" footer
    /// in the trace dialog. Iterates the registry, drains
    /// keys (so we don't double-borrow), unloads each script.
    pub(crate) fn stop_all_traces(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<crate::traces::TraceKey> = self
            .bundle()
            .map(|b| b.traces.entries().iter().map(|e| e.key.clone()).collect())
            .unwrap_or_default();
        for k in keys {
            self.stop_trace(
                k.artifact,
                k.class_jni,
                k.method_name,
                k.method_signature,
                cx,
            );
        }
    }

    // ---- Hook lifecycle ------------------------------------------------

    pub(crate) fn toggle_hooks_dialog(&mut self, cx: &mut Context<Self>) {
        self.hooks_dialog_open = !self.hooks_dialog_open;
        // Always start in list mode — close any editor if it
        // was left open from a previous session.
        self.hook_editor_target = None;
        self.hook_editor_buffer.clear();
        cx.notify();
    }

    pub(crate) fn close_hooks_dialog(&mut self, cx: &mut Context<Self>) {
        if self.hooks_dialog_open {
            self.hooks_dialog_open = false;
            self.hook_editor_target = None;
            self.hook_editor_buffer.clear();
            cx.notify();
        }
    }

    // The four hook-editor methods below wire a future
    // multi-line JS editor pane into the Hooks dialog. The
    // text-input widget is single-line today; once we grow a
    // multi-line variant the dialog's "Edit" button will
    // call open_hook_editor and the editor's commit handler
    // will call save_hook_editor. Leaving the plumbing in
    // place — it's the right shape — but suppressing the
    // dead-code warning until the UI surface exists.
    #[allow(dead_code)]
    /// Switch the hooks dialog into editor mode for one key.
    /// Pre-fills the buffer with the entry's existing JS body
    /// (or a sensible default) so the user can iterate.
    pub(crate) fn open_hook_editor(
        &mut self,
        key: crate::hooks::HookKey,
        cx: &mut Context<Self>,
    ) {
        let initial = self
            .bundle()
            .and_then(|b| b.hooks.get(&key))
            .map(|e| match &e.action {
                crate::hooks::HookAction::CustomJs(body) => body.clone(),
                crate::hooks::HookAction::ReturnLiteral(lit) => {
                    format!("return {lit};")
                }
                crate::hooks::HookAction::LogOnly => {
                    "// runs after the original — return its value\n\
                     return originalImpl.apply(this, args);"
                        .to_string()
                }
            })
            .unwrap_or_else(|| {
                "// args[] are the call's parameters\n\
                 // call originalImpl.apply(this, args) to invoke the\n\
                 // real method, or return a value to override.\n\
                 return originalImpl.apply(this, args);"
                    .to_string()
            });
        self.hook_editor_target = Some(key);
        self.hook_editor_buffer = initial;
        cx.notify();
    }

    #[allow(dead_code)]
    pub(crate) fn close_hook_editor(&mut self, cx: &mut Context<Self>) {
        self.hook_editor_target = None;
        self.hook_editor_buffer.clear();
        cx.notify();
    }

    #[allow(dead_code)]
    /// Persist the editor buffer as a CustomJs hook on the
    /// editor's target key. If the hook doesn't exist yet
    /// (user is creating it fresh), it's started; otherwise
    /// the existing script is unloaded and a new one created
    /// with the new body.
    pub(crate) fn save_hook_editor(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.hook_editor_target.clone() else { return };
        let body = self.hook_editor_buffer.clone();
        // Stop the running hook (if any), then start a fresh
        // one with the new body. This is the simplest "edit"
        // path — Frida sessions don't support live script
        // mutation, so create-replace-on-edit is the model.
        let exists = self
            .bundle()
            .map(|b| b.hooks.get(&key).is_some())
            .unwrap_or(false);
        if exists {
            self.stop_hook(
                key.artifact.clone(),
                key.class_jni.clone(),
                key.method_name.clone(),
                key.method_signature.clone(),
                cx,
            );
        }
        self.start_hook(
            key.artifact.clone(),
            key.class_jni.clone(),
            key.method_name.clone(),
            key.method_signature.clone(),
            crate::hooks::HookAction::CustomJs(body),
            cx,
        );
        self.close_hook_editor(cx);
    }

    #[allow(dead_code)]
    /// Track the editor's buffer. Called by the multi-line
    /// text input on every keystroke.
    pub(crate) fn set_hook_editor_buffer(
        &mut self,
        text: String,
        cx: &mut Context<Self>,
    ) {
        self.hook_editor_buffer = text;
        cx.notify();
    }

    pub(crate) fn start_hook(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature: String,
        action: crate::hooks::HookAction,
        cx: &mut Context<Self>,
    ) {
        let Some(dock) = self.debug_dock.as_ref() else {
            return;
        };
        let Some(session) = dock.session.clone() else {
            self.push_dock_log("• not attached — connect first", cx);
            return;
        };
        let key = crate::hooks::HookKey {
            artifact: artifact.clone(),
            class_jni: class_jni.clone(),
            method_name: method_name.clone(),
            method_signature: method_signature.clone(),
        };
        if let Some(bundle) = self.bundle() {
            if let Some(existing) = bundle.hooks.get(&key) {
                if matches!(
                    existing.status,
                    crate::hooks::HookStatus::Pending
                        | crate::hooks::HookStatus::Active
                ) {
                    self.push_dock_log(
                        format!("• already hooking {class_jni}.{method_name}"),
                        cx,
                    );
                    return;
                }
            }
        }
        if let Some(bundle) = self.bundle_mut() {
            bundle.hooks.remove(&key);
        }
        let body = match &action {
            crate::hooks::HookAction::LogOnly => glass_frida::HookBody::LogOnly,
            crate::hooks::HookAction::ReturnLiteral(lit) => {
                glass_frida::HookBody::ReturnLiteral(lit.clone())
            }
            crate::hooks::HookAction::CustomJs(body) => {
                glass_frida::HookBody::Custom(body.clone())
            }
        };
        let js = match glass_frida::render_hook_script(
            &class_jni,
            &method_name,
            &method_signature,
            &body,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.push_dock_log(format!("• render failed: {e}"), cx);
                return;
            }
        };
        let script_id = session.alloc_script_id();
        if let Some(bundle) = self.bundle_mut() {
            bundle.hooks.insert(crate::hooks::HookEntry {
                key: key.clone(),
                script_id: Some(script_id),
                status: crate::hooks::HookStatus::Pending,
                action,
                created_at: std::time::Instant::now(),
                invocations: Vec::new(),
            });
        }
        self.push_dock_log(
            format!("⚙ hooking {class_jni}.{method_name}{method_signature}"),
            cx,
        );
        cx.notify();
        let name = format!(
            "hook-{}-{}",
            class_jni.replace('/', "."),
            method_name
        );
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    session.create_script(script_id, name, js)
                })
                .await;
            let _ = this.update(cx, |shell, cx| match result {
                Ok(()) => {
                    if let Some(bundle) = shell.bundle_mut() {
                        bundle.hooks.mark_active(&key, script_id);
                    }
                    shell.push_dock_log(
                        format!("• hook {script_id} active"),
                        cx,
                    );
                }
                Err(e) => {
                    if let Some(bundle) = shell.bundle_mut() {
                        bundle.hooks.mark_failed(&key, e.clone());
                    }
                    shell.push_dock_log(
                        format!("• hook {script_id} failed: {e}"),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    pub(crate) fn stop_hook(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature: String,
        cx: &mut Context<Self>,
    ) {
        let key = crate::hooks::HookKey {
            artifact,
            class_jni: class_jni.clone(),
            method_name: method_name.clone(),
            method_signature,
        };
        let script_id = self
            .bundle()
            .and_then(|b| b.hooks.get(&key))
            .and_then(|e| e.script_id);
        if let Some(bundle) = self.bundle_mut() {
            bundle.hooks.remove(&key);
        }
        self.push_dock_log(
            format!("◼ stop hook {class_jni}.{method_name}"),
            cx,
        );
        let Some(session) = self
            .debug_dock
            .as_ref()
            .and_then(|d| d.session.clone())
        else {
            return;
        };
        let Some(id) = script_id else { return };
        cx.spawn(async move |_this, cx| {
            let _ = cx
                .background_executor()
                .spawn(async move {
                    let _ = session.unload_script(id);
                })
                .await;
        })
        .detach();
    }

    pub(crate) fn stop_all_hooks(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<crate::hooks::HookKey> = self
            .bundle()
            .map(|b| b.hooks.entries().iter().map(|e| e.key.clone()).collect())
            .unwrap_or_default();
        for k in keys {
            self.stop_hook(
                k.artifact,
                k.class_jni,
                k.method_name,
                k.method_signature,
                cx,
            );
        }
    }

    /// Smoke test: load a tiny script that calls `send(1+1)`
    /// in the gadget. If the wiring works, the dock's event
    /// pump turns this into a log line like
    /// `[script <id>] 2` within a tick or two. Used to verify
    /// the M3.4 plumbing without any feature code on top.
    pub(crate) fn debug_smoke_test(&mut self, cx: &mut Context<Self>) {
        let Some(dock) = self.debug_dock.as_ref() else { return };
        let Some(session) = dock.session.clone() else {
            self.push_dock_log("not connected — try Connect first", cx);
            return;
        };
        let id = session.alloc_script_id();
        self.push_dock_log(format!("• loading smoke-test script {id}"), cx);
        // Background the create-script call since it blocks
        // on the actor thread; keeps the UI responsive.
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // Diagnostic probe: enumerate the gadget's
                    // global scope so we can see what bridges
                    // are actually present. Sends three lines:
                    //   * runtime + frida.version
                    //   * the global keys (Module, Process,
                    //     Java, ObjC, …)
                    //   * any Java-like candidates we spotted
                    // Smoke test: splice a tiny diagnostic
                    // into the bridge bundle. The same code
                    // path trace/hook scripts use. If this
                    // works, traces will work.
                    let user = r#"
                        send({
                          kind: 'info',
                          stage: 'smoke-after-bridge',
                          typeofJava: typeof Java,
                          javaAvailable: typeof Java !== 'undefined' && Java.available,
                        });
                    "#;
                    let src = glass_frida::build_bridged_script(user);
                    session.create_script(id, "glass-smoke-bridged", src)
                })
                .await;
            let line = match result {
                Ok(()) => format!("smoke script {id} loaded"),
                Err(e) => format!("smoke script {id} failed: {e}"),
            };
            let _ = this.update(cx, |shell, cx| {
                shell.push_dock_log(line, cx);
            });
        })
        .detach();
    }

    /// Start tracing a Java method on the connected gadget.
    /// Inserts a `Pending` entry into the bundle's trace
    /// registry, renders the Frida JS, allocates a script id,
    /// and asks the session actor to load it. On load success
    /// the entry flips to `Active`; on failure to `Failed`.
    ///
    /// No-op (with a log line) when:
    ///   * No bundle is loaded.
    ///   * The dock isn't open / not attached.
    ///   * The method is already being traced.
    pub(crate) fn start_trace(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature: String,
        cx: &mut Context<Self>,
    ) {
        let Some(dock) = self.debug_dock.as_ref() else {
            tracing::info!("start_trace: dock not open — connect first");
            return;
        };
        let Some(session) = dock.session.clone() else {
            self.push_dock_log("• not attached — connect first", cx);
            return;
        };
        let key = crate::traces::TraceKey {
            artifact: artifact.clone(),
            class_jni: class_jni.clone(),
            method_name: method_name.clone(),
            method_signature: method_signature.clone(),
        };
        // Refuse to double-trace ONLY when an active or
        // pending trace is live. Failed / Stopped entries
        // are eligible for retry — the user already saw
        // the failure and clicked Trace again to retry,
        // so let them.
        if let Some(bundle) = self.bundle() {
            if let Some(existing) = bundle.traces.get(&key) {
                if matches!(
                    existing.status,
                    crate::traces::TraceStatus::Pending
                        | crate::traces::TraceStatus::Active
                ) {
                    self.push_dock_log(
                        format!("• already tracing {class_jni}.{method_name}"),
                        cx,
                    );
                    return;
                }
            }
        }
        // Drop any prior Failed/Stopped entry so the
        // insert below replaces it cleanly. mark_failed
        // doesn't update by_script, but remove is the
        // canonical way to clear both indices.
        if let Some(bundle) = self.bundle_mut() {
            bundle.traces.remove(&key);
        }
        // Render JS up front so we fail fast on a bad signature.
        let js = match glass_frida::render_trace_script(
            &class_jni,
            &method_name,
            &method_signature,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.push_dock_log(
                    format!("• render failed: {e}"),
                    cx,
                );
                return;
            }
        };
        let script_id = session.alloc_script_id();
        // Stage Pending → caller's pane can show a "loading"
        // indicator until the actor confirms.
        if let Some(bundle) = self.bundle_mut() {
            bundle.traces.insert(crate::traces::TraceEntry {
                key: key.clone(),
                script_id: Some(script_id),
                status: crate::traces::TraceStatus::Pending,
                created_at: std::time::Instant::now(),
                invocations: Vec::new(),
            });
        }
        self.push_dock_log(
            format!("▶ tracing {class_jni}.{method_name}{method_signature}"),
            cx,
        );
        cx.notify();
        // Load the script off the foreground.
        let name = format!(
            "trace-{}-{}",
            class_jni.replace('/', "."),
            method_name
        );
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    session.create_script(script_id, name, js)
                })
                .await;
            let _ = this.update(cx, |shell, cx| match result {
                Ok(()) => {
                    if let Some(bundle) = shell.bundle_mut() {
                        bundle.traces.mark_active(&key, script_id);
                    }
                    shell.push_dock_log(
                        format!("• trace {script_id} active"),
                        cx,
                    );
                }
                Err(e) => {
                    if let Some(bundle) = shell.bundle_mut() {
                        bundle.traces.mark_failed(&key, e.clone());
                    }
                    shell.push_dock_log(
                        format!("• trace {script_id} failed: {e}"),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Stop and unregister a trace. Removes the entry from
    /// the registry and asks the actor to unload the script.
    /// Cheap when the trace is already gone.
    pub(crate) fn stop_trace(
        &mut self,
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature: String,
        cx: &mut Context<Self>,
    ) {
        let key = crate::traces::TraceKey {
            artifact,
            class_jni: class_jni.clone(),
            method_name: method_name.clone(),
            method_signature,
        };
        // Pull the script id out before we drop the entry.
        let script_id = self
            .bundle()
            .and_then(|b| b.traces.get(&key))
            .and_then(|e| e.script_id);
        if let Some(bundle) = self.bundle_mut() {
            bundle.traces.remove(&key);
        }
        self.push_dock_log(
            format!("◼ stop trace {class_jni}.{method_name}"),
            cx,
        );
        let Some(session) = self
            .debug_dock
            .as_ref()
            .and_then(|d| d.session.clone())
        else {
            return;
        };
        let Some(id) = script_id else { return };
        cx.spawn(async move |_this, cx| {
            let _ = cx
                .background_executor()
                .spawn(async move {
                    let _ = session.unload_script(id);
                })
                .await;
        })
        .detach();
    }

    pub(crate) fn export_patched_bundle(&mut self, cx: &mut Context<Self>) {
        use std::collections::HashMap;
        let Some(bundle) = self.bundle() else { return };
        if bundle.edits.is_empty()
            && bundle.smali_edits.is_empty()
            && bundle.pending_additions.is_empty()
        {
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
        // Parallel map for typed smali class edits, keyed by DEX
        // artifact id (matches the loader's hashing of raw DEX
        // bytes).
        let mut smali_edit_map: glass_api::SmaliEditMap = HashMap::new();
        for e in bundle.smali_edits.entries() {
            smali_edit_map
                .entry(e.key.artifact.clone())
                .or_default()
                .insert(e.key.class_jni.clone(), e.modified.clone());
        }
        // Pending APK additions (new zip entries): clone the
        // bundle's map up front so the post-prompt continuation
        // doesn't need a borrow on Shell.
        let additions: glass_api::ApkAdditions = bundle.pending_additions.clone();
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
            let smali_map_for_task = smali_edit_map.clone();
            let additions_for_task = additions.clone();
            let source_path_for_task = source_path.clone();
            let out_path_for_task = out_path.clone();
            let summary = cx
                .background_executor()
                .spawn(async move {
                    match glass_api::open(&source_path_for_task) {
                        Ok(bundle) => match glass_api::export_to_path_with_smali(
                            &bundle,
                            &edit_map_for_task,
                            &smali_map_for_task,
                            &additions_for_task,
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

    pub(crate) fn bundle_mut(&mut self) -> Option<&mut crate::LoadedBundle> {
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
/// Parse `(name, signature_jni)` out of a `.method` line.
/// Smali shape: `.method <modifiers> [constructor ]<name>(<JNI-sig>)<ret>`.
/// We split at the first `(` to recover the name, then the
/// signature is `(args)ret` from that `(` through end-of-line.
/// Returns `None` if the line doesn't match.
pub(crate) fn parse_method_decl_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix(".method ")?.trim_start();
    // Drop modifier tokens — the name is the last whitespace-
    // separated token *before* the `(`.
    let paren = rest.find('(')?;
    let head = &rest[..paren];
    let sig_part = &rest[paren..];
    let name = head.split_whitespace().last()?;
    if name.is_empty() {
        return None;
    }
    // `.method` lines have nothing after the return type on the
    // same line; safe to take through end-of-string.
    Some((name.to_string(), sig_part.to_string()))
}

/// Parse `(name, signature_jni)` out of a `.field` line.
/// Smali shape: `.field <modifiers> <name>:<JNI-sig> [= <init>]`.
/// Returns `None` if the line doesn't match.
pub(crate) fn parse_field_decl_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix(".field ")?.trim_start();
    // `name:Sig` is the last whitespace-separated token before an
    // optional ` = <init>`. Split off the initial first.
    let head = match rest.find(" = ") {
        Some(eq) => &rest[..eq],
        None => rest,
    };
    let token = head.split_whitespace().last()?;
    let (name, sig) = token.split_once(':')?;
    if name.is_empty() || sig.is_empty() {
        return None;
    }
    Some((name.to_string(), sig.to_string()))
}

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

/// Stdlib type signatures surfaced by the op-edit autocomplete
/// when the user is typing a class ref slot. Bundle classes
/// always take priority — these are only appended if a prefix
/// match isn't already in the loaded DEX. Kept short on purpose;
/// users can type the rest out by hand.
const COMMON_EXTERNAL_TYPES: &[&str] = &[
    "Ljava/lang/Object;",
    "Ljava/lang/String;",
    "Ljava/lang/Integer;",
    "Ljava/lang/Long;",
    "Ljava/lang/Float;",
    "Ljava/lang/Double;",
    "Ljava/lang/Boolean;",
    "Ljava/lang/Byte;",
    "Ljava/lang/Short;",
    "Ljava/lang/Character;",
    "Ljava/lang/Class;",
    "Ljava/lang/Throwable;",
    "Ljava/lang/Exception;",
    "Ljava/lang/RuntimeException;",
    "Ljava/util/List;",
    "Ljava/util/Map;",
    "Ljava/util/Set;",
    "Ljava/util/ArrayList;",
    "Ljava/util/HashMap;",
    "Ljava/util/HashSet;",
    "Ljava/util/Iterator;",
    "Landroid/content/Context;",
    "Landroid/os/Bundle;",
    "Landroid/view/View;",
    "Landroid/util/Log;",
];

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

/// Kind passed to `navigate_to_smali_member` — distinguishes
/// field navigation from method navigation. Lives at file scope
/// so the Changes dialog can name it from its render module.
pub(crate) enum SmaliMemberKind {
    Field { name: String, signature: String },
    Method { name: String, signature: String },
}

/// Per-tick verdict from the foreground to the external-edit
/// polling task. Lives at file scope so the poll method can name
/// the type in its match arms.
enum PollAction {
    /// Session is gone or its identity changed — stop polling.
    Exit,
    /// Nothing observed this tick.
    Continue,
    /// User clicked Stop — clean up the temp file and exit.
    StopAndCleanup,
    /// File changed; read + parse off the foreground using the
    /// observed mtime as the new high-water mark.
    Ingest(std::time::SystemTime),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_field_decl() {
        let (n, s) = parse_field_decl_line(".field private count:I").unwrap();
        assert_eq!(n, "count");
        assert_eq!(s, "I");
    }

    #[test]
    fn parses_field_decl_with_initial() {
        let (n, s) =
            parse_field_decl_line(".field public static final MAX:I = 0x7fffffff").unwrap();
        assert_eq!(n, "MAX");
        assert_eq!(s, "I");
    }

    #[test]
    fn parses_field_decl_with_object_sig() {
        let (n, s) =
            parse_field_decl_line(".field protected name:Ljava/lang/String;").unwrap();
        assert_eq!(n, "name");
        assert_eq!(s, "Ljava/lang/String;");
    }

    #[test]
    fn parses_indented_field_decl() {
        let (n, _) = parse_field_decl_line("    .field public id:I").unwrap();
        assert_eq!(n, "id");
    }

    #[test]
    fn rejects_non_field_lines() {
        assert!(parse_field_decl_line(".class public Lcom/Foo;").is_none());
        assert!(parse_field_decl_line("    invoke-virtual {p0}, …").is_none());
    }

    #[test]
    fn parses_simple_method_decl() {
        let (n, s) =
            parse_method_decl_line(".method public foo()V").unwrap();
        assert_eq!(n, "foo");
        assert_eq!(s, "()V");
    }

    #[test]
    fn parses_constructor_method_decl() {
        let (n, s) = parse_method_decl_line(
            ".method public constructor <init>(Landroid/content/Context;)V",
        )
        .unwrap();
        assert_eq!(n, "<init>");
        assert_eq!(s, "(Landroid/content/Context;)V");
    }

    #[test]
    fn parses_static_method_decl() {
        let (n, s) =
            parse_method_decl_line(".method public static bar(I)Z").unwrap();
        assert_eq!(n, "bar");
        assert_eq!(s, "(I)Z");
    }

    #[test]
    fn rejects_non_method_lines() {
        assert!(parse_method_decl_line(".class public Lcom/Foo;").is_none());
        assert!(parse_method_decl_line(".field private count:I").is_none());
        assert!(parse_method_decl_line("    .end method").is_none());
    }
}
