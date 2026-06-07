//! Navigation cluster: hex / listing / smali cursor movement plus
//! tab + leaf management.
//!
//! Lifted out of `shell_actions.rs` so the file stays under the
//! module-size discipline. The methods are still defined on
//! `Shell` via a sibling `impl Shell` block — Rust allows
//! multiple `impl Shell` blocks across files in the same crate,
//! so the existing call sites continue to work without renames.
//!
//! Scope: byte / row cursor movement on hex, listing and smali
//! tabs (`hex_move_byte`, `move_listing_selection`,
//! `listing_page_scroll`, `select_active_row`, `select_byte`,
//! `scroll_h_by`, `edit_selected_listing_row`,
//! `hex_open_edit_at_selection`), smali class / member
//! navigation (`navigate_to_smali_class`,
//! `navigate_to_smali_member`, `goto_smali_method`), and tab /
//! leaf opening + focus + close (`open_listing_at_addr`,
//! `open_listing_at`, `open_hex_in_new_tab`,
//! `open_hex_force_new_tab`, `open_listing_in_new_tab`,
//! `open_listing_force_new_tab`, `open_leaf`, `focus_tab`,
//! `close_tab`).

use gpui::{px, Context, Pixels};

use crate::shell_actions::SmaliMemberKind;
use crate::{LeafId, LeafKind, Shell, Tab, TabKind};

impl Shell {
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
        let Some(paged) = tab.hex_paged.as_ref().cloned() else { return };
        // Find the (row_index, byte_addr) of the currently
        // selected byte. If there's no selection, start at
        // the first byte of the first Bytes row.
        let (mut row_idx, mut addr) = match tab.selected_byte_addr {
            Some(a) => match paged.row_for_addr(a) {
                Some(i) => (i, a),
                None => return,
            },
            None => {
                let Some(i) = paged.next_byte_row_at_or_after(0) else { return };
                let Some(a) = paged.addr_at(i) else { return };
                (i, a)
            }
        };
        let step: i64 = delta.signum() as i64;
        let new_addr = (addr as i64 + step) as u64;
        let cur_row_addr = match paged.addr_at(row_idx) {
            Some(a) => a,
            None => return,
        };
        if new_addr >= cur_row_addr && new_addr < cur_row_addr + 16 {
            addr = new_addr;
        } else {
            // Cross to the prev/next Bytes row.
            let next_row_idx = if step > 0 {
                paged.next_byte_row_at_or_after(row_idx + 1)
            } else if row_idx == 0 {
                None
            } else {
                paged.prev_byte_row_at_or_before(row_idx - 1)
            };
            let Some(ni) = next_row_idx else { return };
            let Some(ni_addr) = paged.addr_at(ni) else { return };
            row_idx = ni;
            addr = if step > 0 { ni_addr } else { ni_addr + 15 };
        }
        tab.selected_row = Some(row_idx as usize);
        tab.selected_byte_addr = Some(addr);
        tab.scroll.scroll_to_reveal_item(row_idx as usize);
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
            crate::TabKind::Listing { .. } => match tab.listing_paged.as_ref() {
                Some(p) => (p.total_rows() as usize, true),
                None => return,
            },
            crate::TabKind::Hex { .. } => match tab.hex_paged.as_ref() {
                Some(p) => (p.total_rows() as usize, false),
                None => return,
            },
            crate::TabKind::SmaliEditor { .. } => match tab.lines.as_ref() {
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
            // Listing: walk past non-Instruction rows (symbol
            // headers, BB separators) using the paged cache. The
            // helpers materialise a page if it isn't cached;
            // typical key-by-key navigation stays in one page.
            let Some(paged) = tab.listing_paged.as_ref().cloned() else { return };
            let candidate = (pos + step).clamp(0, max) as u32;
            let next_pos = if step > 0 {
                paged.next_instruction_at_or_after(candidate)
            } else {
                paged.prev_instruction_at_or_before(candidate)
            };
            let Some(next_pos) = next_pos else { return };
            pos = next_pos as i32;
            if pos == tab.selected_row.unwrap_or(0) as i32 {
                return;
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
                if let Some(paged) = tab.hex_paged.as_ref().cloned() {
                    // The destination row may be a symbol header
                    // (no address) — skip the cursor update then.
                    if let Some(new_addr) = paged.addr_at(next as u32) {
                        // Preserve the column offset within the
                        // row so vertical movement keeps the
                        // byte cursor under the same column.
                        let column = tab
                            .selected_byte_addr
                            .and_then(|a| paged.row_for_addr(a).and_then(|i| paged.addr_at(i).map(|base| a - base)))
                            .unwrap_or(0);
                        tab.selected_byte_addr = Some(new_addr + column);
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
        let Some(paged) = tab.listing_paged.as_ref().cloned() else { return };
        let Some(address) = paged.addr_at(selected as u32) else { return };
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
        // Reuse the existing open_leaf path so we get tab dedupe.
        // For smali classes this now opens a `SmaliEditor` tab
        // (the read-only viewer is retired), so we move the
        // editor's caret to `line_no` and scroll the viewport
        // to reveal it.
        self.open_leaf(target_leaf, cx);
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.selected_row = Some(line_no);
                tab.pending_smali_scroll_line = Some(line_no);
                if let Some(editor) = tab.code_editor.as_mut() {
                    // Place the caret at column 0 of `line_no`
                    // via the editor's anchor helpers — same
                    // path a click would take.
                    let snap = editor.buffer.snapshot();
                    let max_row = snap.max_point().row;
                    let row = (line_no as u32).min(max_row);
                    let off = snap.point_to_offset(rope::Point::new(row, 0));
                    editor.move_cursor_to_offset(off, false);
                    editor.ensure_caret_visible();
                }
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
        // Smali classes need bundle access to look up the
        // owning artifact, so `TabKind::from_kind` returns None
        // for them and we route through the editor opener.
        let kind = {
            let Some(bundle) = self.bundle() else { return };
            let Some(kind_src) = bundle.kinds.get(leaf.0) else { return };
            if let LeafKind::SmaliClass { class_jni } = kind_src {
                let class_jni = class_jni.clone();
                let _ = bundle;
                self.open_smali_editor_for_class(&class_jni, cx);
                return;
            }
            // Plist leaves go through the editor opener so the
            // CodeEditor is seeded with the XML-form body and a
            // potentially pre-staged edit. The TabKind would
            // open an empty editor otherwise.
            if let LeafKind::Plist { artifact } = kind_src {
                let artifact = artifact.clone();
                let _ = bundle;
                self.open_plist_editor_for_artifact(&artifact, cx);
                return;
            }
            // Manifest leaves take the same editor route as plist:
            // the CodeEditor is seeded with the decoded XML and a
            // pre-staged edit if one exists.
            if let LeafKind::Manifest { artifact } = kind_src {
                let artifact = artifact.clone();
                let _ = bundle;
                self.open_manifest_editor_for_artifact(&artifact, cx);
                return;
            }
            let Some(kind) = TabKind::from_kind(kind_src) else { return };
            kind
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

    /// Open the Coverage Map tab (or focus the existing one).
    /// CoverageMap is a singleton — only one across the shell
    /// at a time.
    pub(crate) fn open_coverage_map(&mut self, cx: &mut Context<Self>) {
        self.overflow_open = false;
        let idx = match self
            .tabs
            .iter()
            .position(|t| matches!(t.kind, TabKind::CoverageMap))
        {
            Some(i) => i,
            None => {
                self.tabs.push(Tab::new(TabKind::CoverageMap));
                self.tabs.len() - 1
            }
        };
        self.active_tab = Some(idx);
        cx.notify();
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
}
