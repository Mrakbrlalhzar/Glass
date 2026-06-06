//! In-window application menu bar.
//!
//! gpui only renders the menus set via `App::set_menus` natively on
//! macOS. On Linux / Windows there is no OS menu bar at all, so the
//! `File` / `View` / app menus defined in [`crate::app::set_app_menus`]
//! would be completely unreachable. This module draws an equivalent
//! menu bar inside the window header (and the dropdown for the open
//! menu) so Open Recent, Theme, Close File, etc. stay available off
//! macOS.
//!
//! Keep the menu contents here in sync with `set_app_menus` in
//! `app.rs` — that one drives the native macOS bar, this one the
//! in-window bar.

use gpui::{div, prelude::*, px, App, Context, MouseButton, SharedString};

use crate::{
    About, CloseFile, CloseWindow, NewWindow, OpenFile, OpenRecent0, OpenRecent1,
    OpenRecent2, OpenRecent3, OpenRecent4, OpenRecent5, OpenRecent6, OpenRecent7,
    OpenRecent8, OpenRecent9, Quit, Shell, Theme0, Theme1, Theme2, Theme3, Theme4,
    Theme5, Theme6, Theme7, TogglePalette,
};

/// Which top-level menu is open. Mirrors the macOS menu set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AppMenu {
    App,
    File,
    View,
}

/// Top-level menus in bar order, with their button labels.
const TOP: [(AppMenu, &str); 3] =
    [(AppMenu::App, "Glass"), (AppMenu::File, "File"), (AppMenu::View, "View")];

/// Mirror of `app::RECENT_SLOTS` / `THEME_SLOTS` — kept local so the
/// menu bar doesn't reach into `app`'s private consts.
const RECENT_SLOTS: usize = 10;
const THEME_SLOTS: usize = 8;

const BTN_W: f32 = 64.0;
const BAR_LEFT: f32 = 4.0;
const BAR_H: f32 = 28.0;

/// One rendered row inside a dropdown.
enum Row {
    Separator,
    /// Dimmed, non-interactive section heading (e.g. "Recent").
    Heading(SharedString),
    Item {
        label: SharedString,
        action: Box<dyn gpui::Action>,
        /// Leading bullet for the active theme, etc.
        checked: bool,
        enabled: bool,
    },
}

fn recent_action(i: usize) -> Box<dyn gpui::Action> {
    match i {
        0 => Box::new(OpenRecent0),
        1 => Box::new(OpenRecent1),
        2 => Box::new(OpenRecent2),
        3 => Box::new(OpenRecent3),
        4 => Box::new(OpenRecent4),
        5 => Box::new(OpenRecent5),
        6 => Box::new(OpenRecent6),
        7 => Box::new(OpenRecent7),
        8 => Box::new(OpenRecent8),
        _ => Box::new(OpenRecent9),
    }
}

fn theme_action(i: usize) -> Box<dyn gpui::Action> {
    match i {
        0 => Box::new(Theme0),
        1 => Box::new(Theme1),
        2 => Box::new(Theme2),
        3 => Box::new(Theme3),
        4 => Box::new(Theme4),
        5 => Box::new(Theme5),
        6 => Box::new(Theme6),
        _ => Box::new(Theme7),
    }
}

fn top_index(menu: AppMenu) -> usize {
    TOP.iter().position(|(m, _)| *m == menu).unwrap_or(0)
}

/// Build the dropdown rows for the open menu, pulling live recents /
/// themes the same way `set_app_menus` does.
fn rows_for(shell: &Shell, menu: AppMenu) -> Vec<Row> {
    match menu {
        AppMenu::App => vec![
            Row::Item {
                label: "About Glass".into(),
                action: Box::new(About),
                checked: false,
                enabled: true,
            },
            Row::Separator,
            Row::Item {
                label: "Quit".into(),
                action: Box::new(Quit),
                checked: false,
                enabled: true,
            },
        ],
        AppMenu::File => {
            let mut rows = vec![Row::Item {
                label: "Open…".into(),
                action: Box::new(OpenFile),
                checked: false,
                enabled: true,
            }];
            rows.push(Row::Separator);
            rows.push(Row::Heading("Open Recent".into()));
            let recents = shell
                .db
                .as_ref()
                .map(|d| d.recent_bundles(RECENT_SLOTS))
                .unwrap_or_default();
            if recents.is_empty() {
                rows.push(Row::Item {
                    label: "No recent files".into(),
                    action: Box::new(OpenRecent0),
                    checked: false,
                    enabled: false,
                });
            } else {
                for (i, rec) in recents.iter().take(RECENT_SLOTS).enumerate() {
                    rows.push(Row::Item {
                        label: rec.label.clone().into(),
                        action: recent_action(i),
                        checked: false,
                        enabled: true,
                    });
                }
            }
            rows.push(Row::Separator);
            rows.push(Row::Item {
                label: "Close File".into(),
                action: Box::new(CloseFile),
                checked: false,
                enabled: true,
            });
            rows.push(Row::Item {
                label: "New Window".into(),
                action: Box::new(NewWindow),
                checked: false,
                enabled: true,
            });
            rows.push(Row::Item {
                label: "Close Window".into(),
                action: Box::new(CloseWindow),
                checked: false,
                enabled: true,
            });
            rows
        }
        AppMenu::View => {
            let mut rows = vec![
                Row::Item {
                    label: "Search…".into(),
                    action: Box::new(TogglePalette),
                    checked: false,
                    enabled: true,
                },
                Row::Separator,
                Row::Heading("Theme".into()),
            ];
            let set = crate::theme::ThemeSet::load();
            let themes = set.all();
            if themes.is_empty() {
                rows.push(Row::Item {
                    label: "No themes installed".into(),
                    action: Box::new(Theme0),
                    checked: false,
                    enabled: false,
                });
            } else {
                for (i, t) in themes.iter().take(THEME_SLOTS).enumerate() {
                    rows.push(Row::Item {
                        label: t.name.clone().into(),
                        action: theme_action(i),
                        checked: t.name == shell.theme.name,
                        enabled: true,
                    });
                }
            }
            rows
        }
    }
}

/// The horizontal strip of top-level menu buttons, rendered at the
/// left of the header. Non-macOS only — callers gate on the platform.
pub(crate) fn render_menu_bar(
    shell: &Shell,
    fg: gpui::Rgba,
    hover_bg: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let open = shell.app_menu_open;
    let mut bar = div().flex().flex_row().items_center().flex_shrink_0();
    for (id, label) in TOP {
        let is_open = open == Some(id);
        let mut btn = div()
            .id(label)
            .w(px(BTN_W))
            .h(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .text_sm()
            .text_color(fg)
            .cursor_pointer()
            .hover(|s| s.bg(hover_bg))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, _w, cx| {
                    this.app_menu_open =
                        if this.app_menu_open == Some(id) { None } else { Some(id) };
                    cx.notify();
                }),
            );
        if is_open {
            btn = btn.bg(hover_bg);
        }
        bar = bar.child(btn);
    }
    bar
}

/// The backdrop + dropdown for the currently-open menu, or `None`
/// when no menu is open. Appended to the window root as an overlay.
pub(crate) fn render_dropdown(
    shell: &Shell,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    hover_bg: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> Option<gpui::AnyElement> {
    let menu = shell.app_menu_open?;
    let rows = rows_for(shell, menu);
    let left = px(BAR_LEFT + top_index(menu) as f32 * BTN_W);

    // Backdrop covers everything *below* the bar so an outside click
    // dismisses, while leaving the bar itself clickable (so the user
    // can switch directly to another top-level menu).
    let backdrop = div()
        .absolute()
        .top(px(BAR_H))
        .left_0()
        .right_0()
        .bottom_0()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                this.app_menu_open = None;
                cx.notify();
            }),
        );

    let mut panel_el = div()
        .absolute()
        .top(px(BAR_H))
        .left(left)
        .min_w(px(200.))
        .py_1()
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_sm()
        .text_color(fg)
        .text_sm()
        .occlude()
        // Eat clicks inside the panel so the backdrop doesn't close
        // it mid-interaction.
        .on_mouse_down(MouseButton::Left, |_ev, _w, cx: &mut App| {
            cx.stop_propagation();
        });

    for (index, row) in rows.into_iter().enumerate() {
        match row {
            Row::Separator => {
                panel_el = panel_el.child(
                    div().my_1().h(px(1.)).bg(border),
                );
            }
            Row::Heading(text) => {
                panel_el = panel_el.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(dim)
                        .child(text),
                );
            }
            Row::Item { label, action, checked, enabled } => {
                // `.id()` turns `Div` into `Stateful<Div>`, so build a
                // common base and collapse both branches to AnyElement.
                let base = div()
                    .px_3()
                    .py_1()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(10.))
                            .text_color(accent)
                            .child(if checked { "●" } else { "" }),
                    )
                    .child(label);
                let item: gpui::AnyElement = if enabled {
                    base.id(("app-menu-item", index))
                        .cursor_pointer()
                        .hover(|s| s.bg(hover_bg))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev, window, cx| {
                                window.dispatch_action(action.boxed_clone(), cx);
                                this.app_menu_open = None;
                                cx.notify();
                            }),
                        )
                        .into_any_element()
                } else {
                    base.text_color(dim).into_any_element()
                };
                panel_el = panel_el.child(item);
            }
        }
    }

    // Full-window transparent wrapper so both children anchor to the
    // window origin. It has no handler / no `occlude`, so clicks on
    // the bar strip (above the backdrop) still reach the buttons.
    Some(
        div()
            .absolute()
            .inset_0()
            .child(backdrop)
            .child(panel_el)
            .into_any_element(),
    )
}
