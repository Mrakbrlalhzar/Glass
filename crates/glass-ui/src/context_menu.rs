//! Floating right-click context menu used by the listing and smali
//! views.
//!
//! State + renderer only. The four `Shell::*_context_menu` methods
//! that build/open/dismiss the menu stay in `lib.rs` because they
//! invoke other Shell methods (`show_cfg`, `show_dex_callgraph`) and
//! consult bundle / symbol data.

use gpui::{div, prelude::*, px, App, Context, Pixels, SharedString};

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
    /// "Copy <label>" — write `text` to the system clipboard.
    /// `label` is the human-readable target descriptor shown to
    /// the right of the menu entry (the address, class name, link
    /// target, etc.); `text` is what actually goes on the
    /// clipboard.
    CopyText {
        text: String,
        label: SharedString,
    },
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
    /// "Open hex view here" — open (or focus) a hex tab for the
    /// text section the right-clicked listing row lives in,
    /// scrolled to `addr`. Lets the user edit the instruction at
    /// the byte level when the typed-assembly editor can't
    /// express what they want.
    OpenHexHere {
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        label: SharedString,
    },
    /// "Open class view" — switch from a listing site that
    /// references an Objective-C method (covered by a synthetic
    /// `-[Class selector:]` symbol) to the class viewer for
    /// `class_name`. The natural reverse of "click the IMP
    /// address in the class view to jump to the listing".
    OpenObjCClass {
        artifact: glass_db::ArtifactId,
        class_name: String,
        label: SharedString,
    },
    /// "Open type view" — Swift equivalent of [`Self::OpenObjCClass`].
    /// Shown when the covering symbol at the right-click site is a
    /// synthetic Swift entry — `type metadata accessor for <T>` or
    /// `<T>.vtable[N]` — both of which carry the raw mangled type
    /// name in the symbol-map's `name` field.
    OpenSwiftType {
        artifact: glass_db::ArtifactId,
        mangled_name: String,
        label: SharedString,
    },
    /// "Revert class edit" — drops the staged smali edit for a
    /// class. Only shown when the active class has a staged edit.
    RevertSmaliClassEdit {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        label: SharedString,
    },
    /// "Revert field edit" — restores a single field on a
    /// staged class to its original lifted version. If that
    /// leaves the class as a whole equal to its original, the
    /// class-level staged edit is dropped too.
    RevertSmaliFieldEdit {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        field_name: String,
        field_signature_jni: String,
        label: SharedString,
    },
    /// "Revert method edit" — analogous to RevertSmaliFieldEdit.
    RevertSmaliMethodEdit {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        label: SharedString,
    },
    /// Start a live Frida trace on the named method. Routed to
    /// `Shell::start_trace`. Shown only when the debug dock is
    /// open and attached.
    StartTrace {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        label: SharedString,
    },
    /// Stop a running trace.
    StopTrace {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        label: SharedString,
    },
    /// Install a hook (log-only initial action). User can
    /// flip it to a return-override via the Hooks dialog.
    StartHook {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        label: SharedString,
    },
    StopHook {
        artifact: glass_db::ArtifactId,
        class_jni: String,
        method_name: String,
        method_signature_jni: String,
        label: SharedString,
    },
    /// "Enable for this bundle" / "Disable for this bundle" — flips
    /// the script's per-bundle enabled flag. The label changes
    /// based on `currently_enabled`; both states use the same
    /// variant so the activator can react with one match arm.
    ToggleScriptEnabled {
        name: String,
        /// State *before* the click. The activator flips the flag.
        currently_enabled: bool,
        label: SharedString,
    },
    /// "Delete script" — removes the `.js` file, metadata, and
    /// every per-bundle enabled row that mentions it. Confirmed
    /// only implicitly today (one click = gone). For an orphan
    /// row (file missing) this is the only way back to a clean
    /// state.
    DeleteScript {
        name: String,
        label: SharedString,
    },
    /// "Cut" — remove the active code editor's selection and
    /// place it on the clipboard. Shown only when there's a
    /// non-empty selection in the active editor tab.
    EditorCut,
    /// "Paste" — insert clipboard text at the caret (replacing
    /// any active selection). Shown only when the clipboard
    /// holds text.
    EditorPaste,
    /// "Edit class header in template…" — opens the templated
    /// class-decl popover for the smali editor's class.
    EditSmaliClassDeclInTemplate,
    /// "Edit field in template…" — opens the templated field
    /// editor seeded from `line` (the `.field …` text).
    EditSmaliFieldInTemplate {
        /// The verbatim `.field` declaration line — passed
        /// straight into `open_field_edit_for_line`.
        line: String,
    },
    /// "Edit method header in template…" — opens the templated
    /// method-header editor seeded from `line` (the `.method …`
    /// declaration text).
    EditSmaliMethodInTemplate {
        line: String,
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
    /// Smali class by leaf — opens (or focuses) the class's
    /// existing tab. Used by the code editor's class-link
    /// follow path.
    SmaliClass {
        leaf: crate::LeafId,
    },
}

pub fn render_context_menu(
    menu: &ContextMenuState,
    viewport: gpui::Size<gpui::Pixels>,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let weak = cx.entity().downgrade();

    // Estimate the menu's height + width so we can flip it
    // above / left of the click when it'd otherwise overflow
    // the viewport. The actual element measures itself at paint
    // time; this is an a-priori approximation good enough for
    // anchoring.
    let est_row_h = gpui::Pixels::from(28.0);
    let est_width = gpui::Pixels::from(260.0);
    let est_height = est_row_h * (menu.items.len() as f32).max(1.0);
    let margin = gpui::Pixels::from(8.0);

    // Default: anchor top-left at the click position. If that
    // overflows the viewport in either axis, flip so the menu
    // sits above / to the left of the click (with a small
    // margin in from the viewport edge so the menu's border
    // doesn't kiss the chrome).
    let max_x = viewport.width - est_width - margin;
    let max_y = viewport.height - est_height - margin;
    let pos_x = if menu.position.x > max_x {
        // Flip left: anchor the menu's right edge at the click,
        // clamped to the viewport's left margin.
        gpui::Pixels::from(0.0).max(menu.position.x - est_width).max(margin)
    } else {
        menu.position.x
    };
    let pos_y = if menu.position.y > max_y {
        // Flip up: anchor the menu's bottom edge at the click.
        gpui::Pixels::from(0.0).max(menu.position.y - est_height).max(margin)
    } else {
        menu.position.y
    };

    let mut panel_el = div()
        .absolute()
        .left(pos_x)
        .top(pos_y)
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
            ContextMenuItem::CopyText { label, .. } => {
                (format!("Copy {label}"), SharedString::from(""))
            }
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
            ContextMenuItem::OpenHexHere { label, .. } => {
                ("Open hex view here".to_string(), label.clone())
            }
            ContextMenuItem::OpenObjCClass { label, .. } => {
                ("Open class view".to_string(), label.clone())
            }
            ContextMenuItem::OpenSwiftType { label, .. } => {
                ("Open type view".to_string(), label.clone())
            }
            ContextMenuItem::RevertSmaliClassEdit { label, .. } => {
                (label.to_string(), SharedString::from(""))
            }
            ContextMenuItem::RevertSmaliFieldEdit { label, .. } => {
                (label.to_string(), SharedString::from(""))
            }
            ContextMenuItem::RevertSmaliMethodEdit { label, .. } => {
                (label.to_string(), SharedString::from(""))
            }
            ContextMenuItem::StartTrace { label, .. } => {
                ("Trace calls".to_string(), label.clone())
            }
            ContextMenuItem::StopTrace { label, .. } => {
                ("Stop tracing".to_string(), label.clone())
            }
            ContextMenuItem::StartHook { label, .. } => {
                ("Hook calls".to_string(), label.clone())
            }
            ContextMenuItem::StopHook { label, .. } => {
                ("Stop hook".to_string(), label.clone())
            }
            ContextMenuItem::ToggleScriptEnabled {
                currently_enabled,
                label,
                ..
            } => {
                let verb = if *currently_enabled {
                    "Disable for this bundle"
                } else {
                    "Enable for this bundle"
                };
                (verb.to_string(), label.clone())
            }
            ContextMenuItem::DeleteScript { label, .. } => {
                ("Delete script".to_string(), label.clone())
            }
            ContextMenuItem::EditorCut => {
                ("Cut".to_string(), SharedString::from(""))
            }
            ContextMenuItem::EditorPaste => {
                ("Paste".to_string(), SharedString::from(""))
            }
            ContextMenuItem::EditSmaliClassDeclInTemplate => (
                "Edit class header in template…".to_string(),
                SharedString::from(""),
            ),
            ContextMenuItem::EditSmaliFieldInTemplate { .. } => (
                "Edit field in template…".to_string(),
                SharedString::from(""),
            ),
            ContextMenuItem::EditSmaliMethodInTemplate { .. } => (
                "Edit method header in template…".to_string(),
                SharedString::from(""),
            ),
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
                        .text_color(crate::theme::current().shell.text_dim.rgba())
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
