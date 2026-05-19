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
    /// Follow a link in-place — reuse the existing same-type tab
    /// (scroll a Listing tab to the address, reuse a Hex tab, etc.).
    /// Same effect as a plain left-click on the link.
    Follow { target: FollowTarget, label: SharedString },
    /// Follow a link in a brand-new tab. Same effect as shift+left-
    /// click on the link.
    FollowInNewTab { target: FollowTarget, label: SharedString },
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
    /// "References to address" — opens the scoped palette with
    /// every caller-site of `addr` in `artifact`.
    XrefsToAddress {
        artifact: glass_db::ArtifactId,
        addr: u64,
        label: SharedString,
    },
    /// "Callers of function" — same as XrefsToAddress but worded
    /// for a function entry point. Identical mechanics.
    CallersOfFunction {
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
        label: SharedString,
    },
    /// "Callers of method" — opens the scoped palette with every
    /// DEX method that invokes `method_key`.
    CallersOfMethod {
        method_key: String,
        label: SharedString,
    },
    /// "References to field" — opens the scoped palette with every
    /// DEX method that touches `field_ref`.
    RefsToField {
        field_ref: String,
        label: SharedString,
    },
    /// "Rename…" / "Edit rename…" — opens the palette as an inline
    /// editor pre-populated with the current value. Currently
    /// unused: the context menu doesn't emit Rename items anymore
    /// (we found they overlapped too much with comments in
    /// practice). The variant + dispatch are kept so that CLI /
    /// MCP `set-rename` writes still display correctly in the
    /// listing and so we can resurrect the menu item cheaply if a
    /// real use case appears.
    #[allow(dead_code)]
    EditRename {
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        current: String,
        label: SharedString,
    },
    /// "Add comment…" / "Edit comment…" — same UX as EditRename
    /// but writes into the comment facet.
    EditComment {
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        current: String,
        label: SharedString,
    },
    /// "Set colour ▸" — opens the swatch popover anchored on the
    /// current row. The popover itself is a separate Shell state
    /// (`colour_picker`), not a sub-menu element.
    PickColour {
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        /// Existing colour, used to mark the currently-selected
        /// swatch when the popover opens.
        current: Option<u32>,
        label: SharedString,
    },
    /// "Clear annotation" — removes every facet (rename / comment
    /// / colour) hung off the key. Only shown when there's
    /// something to clear.
    ClearAnnotation {
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        label: SharedString,
    },
    /// "Revert change" — removes a staged disasm edit at this
    /// address. Only shown when the row has been edited.
    RevertDisasmEdit {
        artifact: glass_db::ArtifactId,
        vaddr: u64,
        label: SharedString,
    },
}

/// Where a Follow / FollowInNewTab action points. Carries the
/// view-type-specific identifiers so the activator can pick the
/// right `Shell::open_*` helper.
#[derive(Clone, Debug)]
pub enum FollowTarget {
    /// Native AArch64 listing at `addr` in `(artifact, section)`.
    Listing {
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
    },
    /// Hex view at `addr` in `(artifact, section)`.
    Hex {
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
    },
    /// Smali method by JNI key, with the resolved leaf + line for
    /// scroll-on-open.
    SmaliMethod {
        leaf: crate::LeafId,
        line: usize,
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
            ContextMenuItem::Follow { label, .. } => {
                ("Follow (left-click)".to_string(), label.clone())
            }
            ContextMenuItem::FollowInNewTab { label, .. } => {
                ("Follow in new tab (⇧+left-click)".to_string(), label.clone())
            }
            ContextMenuItem::ShowCfg { label, .. } => {
                ("Show CFG".to_string(), label.clone())
            }
            ContextMenuItem::ShowDexCallGraph { label, .. } => {
                ("Show call graph".to_string(), label.clone())
            }
            ContextMenuItem::XrefsToAddress { label, .. } => {
                ("References to address".to_string(), label.clone())
            }
            ContextMenuItem::CallersOfFunction { label, .. } => {
                ("Callers of function".to_string(), label.clone())
            }
            ContextMenuItem::CallersOfMethod { label, .. } => {
                ("Callers of method".to_string(), label.clone())
            }
            ContextMenuItem::RefsToField { label, .. } => {
                ("References to field".to_string(), label.clone())
            }
            ContextMenuItem::EditRename { label, .. } => {
                (label.to_string(), SharedString::from(""))
            }
            ContextMenuItem::EditComment { label, .. } => {
                (label.to_string(), SharedString::from(""))
            }
            ContextMenuItem::PickColour { label, .. } => {
                (label.to_string(), SharedString::from(""))
            }
            ContextMenuItem::ClearAnnotation { label, .. } => {
                (label.to_string(), SharedString::from(""))
            }
            ContextMenuItem::RevertDisasmEdit { label, .. } => {
                (label.to_string(), SharedString::from(""))
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
