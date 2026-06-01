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

use crate::hex::{build_hex_rows, hex_row_for_addr};
use crate::listing_model::{build_listing_rows, listing_row_for_addr, DataPeek, ListingRow};
use crate::search::SearchJump;
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
            palette_bin_match_sources: Vec::new(),
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
            frida_server_install: None,
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
                        TabKind::ObjCClass { artifact, class_name } => {
                            return glass_db::TabState::ObjCClass {
                                artifact: artifact.clone(),
                                class_name: class_name.clone(),
                                scroll_line: top_row as u32,
                            };
                        }
                        TabKind::SwiftType { artifact, mangled_name } => {
                            return glass_db::TabState::SwiftType {
                                artifact: artifact.clone(),
                                mangled_name: mangled_name.clone(),
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
                glass_db::TabState::ObjCClass {
                    artifact,
                    class_name,
                    scroll_line,
                } => (
                    TabKind::ObjCClass {
                        artifact: artifact.clone(),
                        class_name: class_name.clone(),
                    },
                    None,
                    if *scroll_line == 0 { None } else { Some(*scroll_line as usize) },
                ),
                glass_db::TabState::SwiftType {
                    artifact,
                    mangled_name,
                    scroll_line,
                } => (
                    TabKind::SwiftType {
                        artifact: artifact.clone(),
                        mangled_name: mangled_name.clone(),
                    },
                    None,
                    if *scroll_line == 0 { None } else { Some(*scroll_line as usize) },
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

    /// Close the currently-loaded bundle and return the window to the
    /// just-launched empty state. Any staged edits / tabs / dialogs
    /// belonging to the bundle are dropped along with the bundle
    /// itself. The window stays open so the Open / Open Recent menu
    /// can repopulate it. No-op when already empty.
    pub(crate) fn close_file(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ShellState::Empty) {
            return;
        }
        self.state = ShellState::Empty;
        self.source_path = None;
        self.progress = None;
        self.tabs.clear();
        self.active_tab = None;
        self.expanded = crate::Expanded::default();
        self.list_state = gpui::ListState::new(0, gpui::ListAlignment::Top, gpui::px(2000.));
        self.visible_count = 0;
        self.tab_bar_width = gpui::px(0.);
        self.overflow_open = false;
        self.section_bar_bounds = gpui::Bounds::default();
        self.hovered_section = None;
        self.bar_cursor_addr = None;
        self.bar_cursor_x = None;
        self.section_table_scroll =
            gpui::ListState::new(0, gpui::ListAlignment::Top, gpui::px(2000.));
        self.section_table_len = 0;
        self.search_index = None;
        self.search_indexing = false;
        self.palette_open = false;
        self.palette_query = crate::text_input::TextInput::new();
        self.palette_selected = 0;
        self.palette_list_state =
            gpui::ListState::new(0, gpui::ListAlignment::Top, gpui::px(2000.));
        self.palette_list_len = 0;
        self.palette_mode = crate::PaletteMode::default();
        self.palette_bin_query = crate::text_input::TextInput::new();
        self.palette_bin_list_state =
            gpui::ListState::new(0, gpui::ListAlignment::Top, gpui::px(2000.));
        self.palette_bin_results = None;
        self.palette_bin_match_sources.clear();
        self.palette_bin_error = None;
        self.palette_bin_artifact = None;
        self.palette_bin_grammar = crate::BinaryGrammar::default();
        self.palette_asm_selected = 0;
        self.palette_asm_candidates.clear();
        self.palette_scope = None;
        self.palette_focused = false;
        self.context_menu = None;
        self.about_open = false;
        self.annotation_edit = None;
        self.colour_picker = None;
        self.disasm_edit = None;
        self.hex_edit = None;
        self.class_decl_edit = None;
        self.field_edit = None;
        self.method_edit = None;
        self.op_edit = None;
        self.annotation_stack = None;
        self.external_edit = None;
        self.frida_probes.clear();
        self.injection_dialog = None;
        self.injection_progress = None;
        self.debug_dock = None;
        self.debug_dock_resize_anchor = None;
        self.traces_dialog_open = false;
        self.hooks_dialog_open = false;
        self.hook_editor_target = None;
        self.hook_editor_buffer.clear();
        self.changes_dialog_open = false;
        self.changes_dialog_confirm_abandon = false;
        self.export_status = None;
        self.export_in_progress = false;
        self.window_tint = 0;
        cx.notify();
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
            TabKind::ObjCClass { class_name, .. } => self
                .tab_leaf(index)
                .and_then(|LeafId(i)| bundle.labels.get(i).cloned())
                .unwrap_or_else(|| SharedString::from(class_name.clone())),
            TabKind::SwiftType { mangled_name, .. } => self
                .tab_leaf(index)
                .and_then(|LeafId(i)| bundle.labels.get(i).cloned())
                .unwrap_or_else(|| SharedString::from(mangled_name.clone())),
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
        symbols: Arc<glass_arch_arm::SymbolMap>,
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
                    // Apply any pending scroll request now that rows
                    // exist. Leave the pending addr in place so
                    // `ensure_active_tab_lines` re-applies it on the
                    // next paint once the viewport has real bounds —
                    // otherwise the first scroll can land short
                    // because `scroll_into_view_with_context` reads
                    // zero viewport height.
                    if let Some(addr) = tab.pending_scroll_addr {
                        if let Some(row_idx) = listing_row_for_addr(rows.as_ref(), addr)
                        {
                            scroll_into_view_with_context(&tab.scroll, row_idx);
                            tab.selected_row = Some(row_idx);
                        } else {
                            tab.pending_scroll_addr = None;
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
                // `hex_view_section` looks up the data section first
                // and falls through to `text_sections` if the user
                // asked for `.text` (the "Open hex view here" context
                // menu does this). We hold an owned `DataSectionBytes`
                // either way; the Arc inside is the only real cost.
                let Some(data) = bundle.hex_view_section(artifact, section) else {
                    return;
                };
                if tab.hex_rows.is_none() {
                    let empty = glass_arch_arm::SymbolMap::default();
                    let symbols = bundle.symbol_maps.get(artifact).unwrap_or(&empty);
                    let rows = build_hex_rows(&data, symbols);
                    tab.scroll = ListState::new(rows.len(), ListAlignment::Top, px(2000.));
                    tab.hex_rows = Some(Arc::new(rows));
                }
                // Pending scroll-to address — typically from a palette
                // search hit ("string" in rodata, etc.) or a follow
                // from a Listing's resolved-symbol comment. We hold
                // the pending addr until the list element has a real
                // viewport (`viewport_bounds().size.height > 0`) so
                // `scroll_into_view_with_context` lands at the right
                // place. On the very first paint after the tab was
                // created, the viewport is still zero and the scroll
                // either clamps weirdly or lands without enough
                // context above the target. Peeking + retrying on the
                // next paint, then taking, fixes both.
                if let Some(addr) = tab.pending_scroll_addr {
                    if let Some(rows) = tab.hex_rows.as_ref() {
                        if let Some(idx) = hex_row_for_addr(rows.as_ref(), addr) {
                            let viewport_ready =
                                tab.scroll.viewport_bounds().size.height > px(0.);
                            scroll_into_view_with_context(&tab.scroll, idx);
                            tab.selected_row = Some(idx);
                            tab.selected_byte_addr = Some(addr);
                            if viewport_ready {
                                tab.pending_scroll_addr = None;
                            } else {
                                cx.notify();
                            }
                        } else {
                            tab.pending_scroll_addr = None;
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
                    let empty = glass_arch_arm::SymbolMap::default();
                    let symbols_arc: Arc<glass_arch_arm::SymbolMap> = Arc::new(
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
                    // Code sections — needed only by `peek_u32_le` for
                    // ARMv7 Thumb literal-pool dereference (the pool
                    // word sits inside `.text`, between functions, and
                    // holds a 32-bit pointer into rodata). `peek_string`
                    // stays data-only.
                    let mut code_sections = Vec::new();
                    for ((aid, _name), ts) in bundle.text_sections.iter() {
                        if aid != &artifact {
                            continue;
                        }
                        code_sections.push((ts.base, ts.bytes.clone()));
                    }
                    let data_arc = Arc::new(DataPeek {
                        sections: data_sections,
                        code_sections,
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
                    if let Some(addr) = tab.pending_scroll_addr {
                        if let Some(rows) = tab.listing_rows.as_ref() {
                            if let Some(idx) = listing_row_for_addr(rows.as_ref(), addr) {
                                let viewport_ready =
                                    tab.scroll.viewport_bounds().size.height > px(0.);
                                scroll_into_view_with_context(&tab.scroll, idx);
                                tab.selected_row = Some(idx);
                                if viewport_ready {
                                    tab.pending_scroll_addr = None;
                                } else {
                                    cx.notify();
                                }
                            } else {
                                tab.pending_scroll_addr = None;
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
            // ObjC class: rows are precomputed at load time into
            // `bundle.objc_classes`. Size the scroll state once on
            // first activation; the renderer walks the cached rows
            // directly. Reuse `lines` as a "did initial setup"
            // marker (Manifest does the same).
            TabKind::ObjCClass { artifact, class_name } => {
                let key = (artifact.clone(), class_name.clone());
                let len = bundle
                    .objc_classes
                    .get(&key)
                    .map(|r| r.len())
                    .unwrap_or(0);
                if tab.lines.is_none() {
                    tab.scroll = ListState::new(len, ListAlignment::Top, px(2000.));
                    tab.lines = Some(Arc::new(Vec::new()));
                }
            }
            // Swift type: same shape as ObjC — rows precomputed at
            // load time into `bundle.swift_types`; size scroll state
            // once on first activation.
            TabKind::SwiftType { artifact, mangled_name } => {
                let key = (artifact.clone(), mangled_name.clone());
                let len = bundle
                    .swift_types
                    .get(&key)
                    .map(|r| r.len())
                    .unwrap_or(0);
                if tab.lines.is_none() {
                    tab.scroll = ListState::new(len, ListAlignment::Top, px(2000.));
                    tab.lines = Some(Arc::new(Vec::new()));
                }
            }
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
    pub(crate) fn refresh_artifact_annotations(
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

    // ---- Disasm-row instruction editor moved to `crate::editor` ---
    // Methods (still on `Shell` via a sibling `impl Shell` block in
    // that module): begin_disasm_edit_at_address, begin_disasm_edit,
    // disasm_edit_handle_key, click_disasm_suggestion,
    // move_disasm_suggestion(_pub), commit_disasm_suggestion(_pub),
    // refresh_disasm_edit_suggestions, commit_disasm_edit,
    // cancel_disasm_edit, revert_disasm_edit. Call sites use the
    // same `self.foo(…)` syntax — no renames needed.

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
            absorbed_following: 0,
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

}

/// Best-effort short ASCII preview of the string at `addr`, used
/// as the chip label for "References to ..." menu items pointing
/// at strings-section addresses. Returns `None` when the address
/// isn't in a strings section, when the byte before the address
/// isn't a NUL (i.e. it's mid-string), or when the string is
/// non-printable.
pub(crate) fn preview_string_at(
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

pub(crate) fn build_native_xref_entries(
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

pub(crate) fn build_dex_caller_entries(
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

pub(crate) fn build_dex_field_entries(
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

/// Stdlib type signatures surfaced by the op-edit autocomplete
/// when the user is typing a class ref slot. Bundle classes
/// always take priority — these are only appended if a prefix
/// match isn't already in the loaded DEX. Kept short on purpose;
/// users can type the rest out by hand.
pub(crate) const COMMON_EXTERNAL_TYPES: &[&str] = &[
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
