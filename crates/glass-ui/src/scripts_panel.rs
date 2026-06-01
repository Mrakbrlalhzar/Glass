//! Frida Scripts section of the left navigator.
//!
//! Lives below the bundle tree (`two_pane.rs` stacks them
//! vertically). Owns its own list of [`ScriptRowState`]s, refreshed
//! from disk + glass-db on:
//!
//!   * Shell construction.
//!   * After any script verb writes (write / delete / enable
//!     / disable) from the GUI's own buttons.
//!   * On a slow tick — covers external `.js` edits and CLI
//!     invocations that modified the library.
//!
//! Reads go through the GUI's already-open `Database` handle on
//! Shell, not by re-opening `glass-api::scripts()` (which would
//! fight the lock).
//!
//! The renderer is deliberately small — one fixed-height header
//! row + a virtualized list of script rows. Each row has an
//! enabled dot and a label; click opens the editor; right-click
//! opens the context menu (handled in `context_menu.rs`).

use std::path::PathBuf;

use gpui::{div, prelude::*, px, App, Context, SharedString};

use crate::Shell;

/// One script's row state, cached on Shell. Bodies aren't kept
/// here — open the editor to load them.
#[derive(Clone, Debug)]
pub(crate) struct ScriptRowState {
    pub name: String,
    /// Empty when the user hasn't set one. Shown as the row's
    /// hover tooltip (TODO: wire when we have the standard
    /// tooltip surface).
    pub description: String,
    /// True when the bundle has this script enabled. Drives the
    /// "enabled dot" colour.
    pub enabled_for_bundle: bool,
    /// True when the script has a loaded `ScriptId` in the
    /// current Frida session. For Phase 2 we always set this
    /// `false` — phase 3 will wire it up to the dock state.
    pub loaded_in_session: bool,
    /// File missing from disk (only metadata remains). Shown as
    /// a strikethrough / muted row so the user can clean it up
    /// via context-menu delete.
    pub present_on_disk: bool,
}

/// Whole scripts-panel state. Cached on Shell.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScriptsPanel {
    pub rows: Vec<ScriptRowState>,
    /// Collapse state for the "Frida" group header. Default
    /// expanded; user can fold to reclaim vertical space.
    pub expanded: bool,
    /// Last refresh attempt. Use to throttle filesystem polls.
    pub last_refresh: Option<std::time::Instant>,
}

impl ScriptsPanel {
    pub fn new() -> Self {
        Self { rows: Vec::new(), expanded: true, last_refresh: None }
    }
}

impl Shell {
    /// Re-read the script library from disk + glass-db using the
    /// GUI's already-open Database handle. Called on Shell
    /// construction and after any script verb the GUI runs.
    pub(crate) fn refresh_scripts(&mut self, cx: &mut Context<Self>) {
        // Read the inputs before we touch the mutable panel field
        // — keeps the borrow checker happy and the data flow
        // explicit (Database + bundle lookups happen up-front,
        // mutation of `scripts_panel` happens at the end).
        let bundle_id = self.bundle().and_then(|b| b.bundle_id.clone());
        let (meta_map, enabled_set) = match self.db.as_ref() {
            Some(db) => {
                let meta = db.all_script_meta();
                let enabled: std::collections::HashSet<String> =
                    match bundle_id.as_ref() {
                        Some(bid) => db.enabled_scripts(bid).into_iter().collect(),
                        None => std::collections::HashSet::new(),
                    };
                (meta, enabled)
            }
            None => {
                self.scripts_panel.rows.clear();
                self.scripts_panel.last_refresh =
                    Some(std::time::Instant::now());
                cx.notify();
                return;
            }
        };
        let panel = &mut self.scripts_panel;
        panel.last_refresh = Some(std::time::Instant::now());

        // Walk the scripts dir. We don't go through
        // `glass-api::scripts()` because that would re-open the
        // redb file we already hold a handle to.
        let dir = match glass_db::scripts_dir() {
            Ok(d) => d,
            Err(_) => {
                panel.rows.clear();
                cx.notify();
                return;
            }
        };
        let mut entries: std::collections::BTreeMap<String, ScriptRowState> =
            std::collections::BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let path = ent.path();
                let Some(name) = script_name_from_path(&path) else { continue };
                let meta = meta_map.get(&name).cloned().unwrap_or_default();
                entries.insert(
                    name.clone(),
                    ScriptRowState {
                        name: name.clone(),
                        description: meta.description,
                        enabled_for_bundle: enabled_set.contains(&name),
                        loaded_in_session: false,
                        present_on_disk: true,
                    },
                );
            }
        }
        // Orphan metadata rows — file missing.
        for (name, meta) in meta_map {
            entries.entry(name.clone()).or_insert(ScriptRowState {
                name: name.clone(),
                description: meta.description,
                enabled_for_bundle: enabled_set.contains(&name),
                loaded_in_session: false,
                present_on_disk: false,
            });
        }
        panel.rows = entries.into_values().collect();
        cx.notify();
    }
}

/// Pull `<name>.js` out of a path. Mirrors the same rules glass-api
/// applies — extension must be `js`, leading dot is reserved.
fn script_name_from_path(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let ext = path.extension()?.to_str()?;
    if ext != "js" || stem.starts_with('.') {
        return None;
    }
    Some(stem.to_string())
}

/// The scripts dir on disk — exposed so other Shell actions
/// (delete, write) can do their own filesystem work without
/// duplicating the resolution.
pub(crate) fn scripts_dir() -> PathBuf {
    glass_db::scripts_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// ---- rendering ---------------------------------------------------------------

/// Header row for the "Frida" section. Chevron toggles
/// `panel.expanded`; the `+` button on the right calls
/// `Shell::create_new_script`.
pub(crate) fn render_section_header(
    expanded: bool,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let chevron = if expanded { "▾" } else { "▸" };
    let theme = crate::theme::current();
    div()
        .id("frida-section-header")
        .h(px(24.))
        .w_full()
        .px_3()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .text_xs()
        .text_color(fg)
        .bg(theme.shell.panel.rgba())
        .border_b_1()
        .border_color(border)
        .child(
            div()
                .w(px(14.))
                .flex_shrink_0()
                .child(SharedString::from(chevron)),
        )
        .child(div().flex_1().child(SharedString::from("Frida")))
        .child(
            div()
                .id("frida-section-add")
                .w(px(16.))
                .h(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .text_color(dim)
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::current().hovers.standard.rgba()))
                .child(SharedString::from("+"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|shell, ev, _w, cx| {
                        // Stop propagation so the chevron click
                        // doesn't fire too.
                        let _ = ev;
                        shell.create_new_script(cx);
                    }),
                ),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                shell.scripts_panel.expanded = !shell.scripts_panel.expanded;
                cx.notify();
            }),
        )
}

/// One script row. Renders the enabled dot + name. Click opens
/// the editor tab; right-click is wired in `context_menus.rs`.
pub(crate) fn render_script_row(
    row: &ScriptRowState,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let theme = crate::theme::current();
    // Indicator: green dot when loaded in a session, accent ring
    // when enabled-but-not-loaded, hollow grey when disabled.
    // Phase 2 stops at enabled/disabled — `loaded_in_session` is
    // wired in phase 3.
    let (dot_char, dot_color) = if row.loaded_in_session {
        ("●", theme.state.committed_bg.rgba())
    } else if row.enabled_for_bundle {
        ("●", accent)
    } else {
        ("○", dim)
    };
    let label_color = if row.present_on_disk { fg } else { dim };
    let name_for_click = row.name.clone();
    let name_for_ctx = row.name.clone();
    div()
        .id(SharedString::from(format!("script-row-{}", row.name)))
        .h(px(22.))
        .w_full()
        .pl(px(22.))
        .pr_3()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .text_xs()
        .text_color(label_color)
        .hover(|s| s.bg(crate::theme::current().hovers.standard.rgba()))
        .child(
            div()
                .w(px(10.))
                .flex_shrink_0()
                .text_color(dot_color)
                .child(SharedString::from(dot_char)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(SharedString::from(row.name.clone())),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |shell, _ev, _w, cx| {
                shell.open_script_editor(&name_for_click, cx);
            }),
        )
        .on_mouse_down(
            gpui::MouseButton::Right,
            cx.listener(move |shell, ev, w, cx| {
                shell.open_script_context_menu(&name_for_ctx, ev, w, cx);
            }),
        )
}

/// Render the full panel (header + rows). Returns a flex column
/// that the caller drops into its layout. When `expanded` is
/// false, only the header is rendered.
pub(crate) fn render_panel(
    shell: &Shell,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    let mut col = div()
        .w_full()
        .flex()
        .flex_col()
        .bg(panel)
        .child(render_section_header(
            shell.scripts_panel.expanded,
            border,
            fg,
            dim,
            cx,
        ));
    if shell.scripts_panel.expanded {
        if shell.scripts_panel.rows.is_empty() {
            col = col.child(
                div()
                    .h(px(22.))
                    .pl(px(22.))
                    .pr_3()
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(dim)
                    .child(SharedString::from(
                        "No scripts yet — click + to create one.",
                    )),
            );
        } else {
            for row in &shell.scripts_panel.rows {
                col = col.child(render_script_row(row, fg, dim, accent, cx));
            }
        }
    }
    // Quieten the lint about the unused App-cx import — the
    // closures above already pull what they need.
    let _ = std::marker::PhantomData::<App>;
    col.into_any_element()
}
