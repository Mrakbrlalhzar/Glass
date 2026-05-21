//! Modal dialog listing every active Frida hook.
//!
//! Mirrors `traces_dialog`: 720 px card, dim backdrop. Each
//! row shows class.method, action summary, hit count, plus
//! per-row Stop. Footer: Stop all + Close.
//!
//! The Edit affordance for custom JS will follow once we have
//! a multi-line text input in the UI. For now the dialog
//! offers four preset actions (Log, Return true/false/null)
//! that cover the bulk of practical use cases.

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};

use crate::hooks::{HookAction, HookEntry, HookStatus};
use crate::Shell;

pub fn render_hooks_dialog(
    entries: &[HookEntry],
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let header = div()
        .text_lg()
        .text_color(fg)
        .child(SharedString::from("Live hooks"));
    let body = if entries.is_empty() {
        div()
            .py_4()
            .text_sm()
            .text_color(dim)
            .child(SharedString::from(
                "No active hooks. Right-click a method in the smali view → Hook calls.",
            ))
    } else {
        let mut col = div().flex().flex_col().gap_2();
        for entry in entries {
            col = col.child(hook_row(entry, fg, dim, border, cx));
        }
        col
    };
    let stop_all = if entries.is_empty() {
        None
    } else {
        Some(
            div()
                .id("hooks-dialog-stop-all")
                .px_3()
                .py_1p5()
                .border_1()
                .border_color(border)
                .rounded_sm()
                .text_sm()
                .text_color(fg)
                .cursor_pointer()
                .child(SharedString::from("Stop all"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|shell, _ev, _w, cx| {
                        shell.stop_all_hooks(cx);
                    }),
                ),
        )
    };
    let close = div()
        .id("hooks-dialog-close")
        .px_3()
        .py_1p5()
        .border_1()
        .border_color(accent)
        .rounded_sm()
        .text_sm()
        .text_color(fg)
        .cursor_pointer()
        .child(SharedString::from("Close"))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                shell.close_hooks_dialog(cx);
            }),
        );

    let card = div()
        .id("hooks-dialog-card")
        .w(px(720.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_md()
        .shadow_lg()
        .p_5()
        .flex()
        .flex_col()
        .gap_3()
        .occlude()
        .child(header)
        .child(body)
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .justify_end()
                .when_some(stop_all, |d, btn| d.child(btn))
                .child(close),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        );

    div()
        .absolute()
        .inset_0()
        .bg(crate::theme::current().modals.overlay_light.rgba())
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                shell.close_hooks_dialog(cx);
            }),
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}

fn hook_row(
    entry: &HookEntry,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let class_short = entry
        .key
        .class_jni
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .map(|s| s.replace('/', "."))
        .unwrap_or_else(|| entry.key.class_jni.clone());
    let title = format!(
        "{}.{}{}",
        class_short, entry.key.method_name, entry.key.method_signature
    );
    let status = match &entry.status {
        HookStatus::Pending => "pending".to_string(),
        HookStatus::Active => format!("{} hit(s)", entry.invocations.len()),
        HookStatus::Failed { message } => format!("failed: {message}"),
        HookStatus::Stopped => "stopped".to_string(),
    };
    let artifact = entry.key.artifact.clone();
    let class_jni = entry.key.class_jni.clone();
    let method_name = entry.key.method_name.clone();
    let method_signature = entry.key.method_signature.clone();
    let id = format!(
        "hooks-dialog-row-{}-{}",
        entry.key.class_jni, entry.key.method_name
    );
    let stop_artifact = artifact.clone();
    let stop_class = class_jni.clone();
    let stop_method = method_name.clone();
    let stop_sig = method_signature.clone();
    let cycle_artifact = artifact.clone();
    let cycle_class = class_jni.clone();
    let cycle_method = method_name.clone();
    let cycle_sig = method_signature.clone();
    let current_action = entry.action.clone();
    let action_label = current_action.summary();
    div()
        .id(SharedString::from(id))
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .py_1p5()
        .border_b_1()
        .border_color(border)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .text_color(fg)
                        .font_family("Courier New")
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(dim)
                        .child(SharedString::from(action_label)),
                ),
        )
        .child(
            div()
                .w(px(120.))
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(status)),
        )
        // "Cycle" button — rotates through Log → Return
        // true → Return false → Return null → Log. Stops
        // the old script + starts a fresh one with the
        // new action. Saves a JS editor's worth of UX
        // for the v1 dialog.
        .child(
            div()
                .id("hooks-dialog-row-cycle")
                .px_2()
                .py_0p5()
                .border_1()
                .border_color(border)
                .rounded_sm()
                .text_xs()
                .text_color(fg)
                .cursor_pointer()
                .child(SharedString::from("Cycle"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |shell, _ev, _w, cx| {
                        let next = next_action(&current_action);
                        shell.stop_hook(
                            cycle_artifact.clone(),
                            cycle_class.clone(),
                            cycle_method.clone(),
                            cycle_sig.clone(),
                            cx,
                        );
                        shell.start_hook(
                            cycle_artifact.clone(),
                            cycle_class.clone(),
                            cycle_method.clone(),
                            cycle_sig.clone(),
                            next,
                            cx,
                        );
                    }),
                ),
        )
        .child(
            div()
                .id("hooks-dialog-row-stop")
                .px_2()
                .py_0p5()
                .border_1()
                .border_color(border)
                .rounded_sm()
                .text_xs()
                .text_color(fg)
                .cursor_pointer()
                .child(SharedString::from("Stop"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |shell, _ev, _w, cx| {
                        shell.stop_hook(
                            stop_artifact.clone(),
                            stop_class.clone(),
                            stop_method.clone(),
                            stop_sig.clone(),
                            cx,
                        );
                    }),
                ),
        )
}

/// Rotate through the four built-in actions. Cheap UX for
/// flipping a hook between observe and override without a
/// JS editor. Order: Log → return true → return false →
/// return null → back to Log.
fn next_action(current: &HookAction) -> HookAction {
    match current {
        HookAction::LogOnly => HookAction::ReturnLiteral("true".into()),
        HookAction::ReturnLiteral(lit) if lit == "true" => {
            HookAction::ReturnLiteral("false".into())
        }
        HookAction::ReturnLiteral(lit) if lit == "false" => {
            HookAction::ReturnLiteral("null".into())
        }
        HookAction::ReturnLiteral(_) => HookAction::LogOnly,
        HookAction::CustomJs(body) => HookAction::CustomJs(body.clone()),
    }
}
