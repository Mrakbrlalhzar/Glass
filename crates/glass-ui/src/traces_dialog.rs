//! Modal dialog listing every active Frida trace.
//!
//! Opened from the debug-dock header's "Traces…" button. Same
//! shape as `changes_dialog` and `injection_dialog`: a 720 px
//! card centred on a dim backdrop, click outside to dismiss.
//! Each row shows the traced method + its invocation count +
//! a per-row Stop button; footer carries a Stop-all action.

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};

use crate::traces::{TraceEntry, TraceStatus};
use crate::Shell;

pub fn render_traces_dialog(
    traces: &[TraceEntry],
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
        .child(SharedString::from("Live traces"));
    let body = if traces.is_empty() {
        div()
            .py_4()
            .text_sm()
            .text_color(dim)
            .child(SharedString::from(
                "No active traces. Right-click a method in the smali view → Trace calls.",
            ))
    } else {
        let mut col = div().flex().flex_col().gap_2();
        for entry in traces {
            col = col.child(trace_row(entry, fg, dim, border, cx));
        }
        col
    };
    let stop_all = if traces.is_empty() {
        None
    } else {
        Some(
            div()
                .id("traces-dialog-stop-all")
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
                        shell.stop_all_traces(cx);
                    }),
                ),
        )
    };
    let close = div()
        .id("traces-dialog-close")
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
                shell.close_traces_dialog(cx);
            }),
        );

    let card = div()
        .id("traces-dialog-card")
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
                shell.close_traces_dialog(cx);
            }),
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}

fn trace_row(
    entry: &TraceEntry,
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
        TraceStatus::Pending => "pending".to_string(),
        TraceStatus::Active => format!("{} hit(s)", entry.invocations.len()),
        TraceStatus::Failed { message } => format!("failed: {message}"),
        TraceStatus::Stopped => "stopped".to_string(),
    };
    // Capture the key fields so the Stop button's listener
    // can fire `stop_trace` without dragging a borrow on
    // entry.
    let artifact = entry.key.artifact.clone();
    let class_jni = entry.key.class_jni.clone();
    let method_name = entry.key.method_name.clone();
    let method_signature = entry.key.method_signature.clone();
    let id = format!(
        "traces-dialog-row-{}-{}",
        entry.key.class_jni, entry.key.method_name
    );
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
                .text_sm()
                .text_color(fg)
                .font_family("Courier New")
                .whitespace_nowrap()
                .child(SharedString::from(title)),
        )
        .child(
            div()
                .w(px(120.))
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(status)),
        )
        .child(
            div()
                .id("traces-dialog-row-stop")
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
                        shell.stop_trace(
                            artifact.clone(),
                            class_jni.clone(),
                            method_name.clone(),
                            method_signature.clone(),
                            cx,
                        );
                    }),
                ),
        )
}
