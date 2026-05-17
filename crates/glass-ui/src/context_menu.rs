//! Floating right-click context menu used by the listing and smali
//! views.
//!
//! State + renderer only. The four `Shell::*_context_menu` methods
//! that build/open/dismiss the menu stay in `lib.rs` because they
//! invoke other Shell methods (`show_cfg`, `show_dex_callgraph`) and
//! consult bundle / symbol data.

use gpui::{div, prelude::*, px, rgb, App, Context, Pixels, SharedString};

use crate::Shell;

/// Floating context menu summoned by right-click on a listing row.
/// Position is in window coordinates; the renderer offsets a panel by
/// these.
#[derive(Clone)]
pub struct ContextMenuState {
    pub position: gpui::Point<Pixels>,
    pub items: Vec<ContextMenuItem>,
}

#[derive(Clone, Debug)]
pub enum ContextMenuItem {
    /// Open the CFG view for the function whose entry is `entry_addr`.
    /// `label` is the demangled function name shown in the menu item.
    ShowCfg {
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
        label: SharedString,
    },
    /// Open the DEX method call-graph view rooted on this method.
    ShowDexCallGraph {
        class_jni: String,
        method_decl: String,
        label: SharedString,
    },
}

pub fn render_context_menu(
    menu: &ContextMenuState,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let weak = cx.entity().downgrade();

    let mut panel_el = div()
        .absolute()
        .left(menu.position.x)
        .top(menu.position.y)
        .min_w(px(220.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_sm()
        .text_color(fg)
        .text_sm()
        .font_family("Menlo")
        .occlude()
        // Eat clicks inside the menu so the backdrop's
        // dismiss-on-click handler doesn't fire when the user
        // moves between items.
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        );

    for (index, item) in menu.items.iter().enumerate() {
        let (label_text, hint) = match item {
            ContextMenuItem::ShowCfg { label, .. } => {
                ("Show CFG".to_string(), label.clone())
            }
            ContextMenuItem::ShowDexCallGraph { label, .. } => {
                ("Show call graph".to_string(), label.clone())
            }
        };
        let weak = weak.clone();
        panel_el = panel_el.child(
            div()
                .id(("context-menu-item", index))
                .px_3()
                .py_2()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .cursor_pointer()
                .hover(|s| s.bg(accent))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |_ev, _w, cx: &mut App| {
                        cx.stop_propagation();
                        if let Some(entity) = weak.upgrade() {
                            cx.update_entity(&entity, |shell, cx| {
                                shell.activate_context_menu_item(index, cx);
                            });
                        }
                    },
                )
                .child(div().child(label_text))
                .child(
                    div()
                        .flex_1()
                        .text_color(rgb(0x808088))
                        .child(hint),
                ),
        );
    }

    let weak_for_backdrop = cx.entity().downgrade();
    let weak_for_right = cx.entity().downgrade();
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |_ev, _w, cx: &mut App| {
                if let Some(entity) = weak_for_backdrop.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.close_context_menu(cx);
                    });
                }
            },
        )
        .on_mouse_down(
            gpui::MouseButton::Right,
            move |_ev, _w, cx: &mut App| {
                if let Some(entity) = weak_for_right.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.close_context_menu(cx);
                    });
                }
            },
        )
        .child(panel_el)
}
