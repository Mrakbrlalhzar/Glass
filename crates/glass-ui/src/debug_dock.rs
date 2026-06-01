//! Bottom debug-dock renderer.
//!
//! Renders the persistent strip at the bottom of the body when
//! `Shell.debug_dock` is `Some`. Layout:
//!
//!   ┌─ drag handle ────────────────────────────────────┐
//!   │  com.example.app · Pixel 6 · frida-gadget 17.9  ⨯│
//!   ├──────────────────────────────────────────────────┤
//!   │  [↻ Restart]  [◼ Stop]                          │
//!   ├──────────────────────────────────────────────────┤
//!   │  ▶ launching com.example.app                     │
//!   │  ** Starting: Intent { …}                        │
//!   │  …                                               │
//!   └──────────────────────────────────────────────────┘
//!
//! Future expansions (trace pane, logcat, screenshot) plug in
//! by adding rows alongside the existing controls.

use gpui::{div, prelude::*, px, AnyElement, Context, SharedString};

use crate::{DebugDockState, Shell};

const DOCK_HANDLE_PX: f32 = 6.;

pub fn render_debug_dock(
    state: &DebugDockState,
    traces: &[crate::traces::TraceEntry],
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let theme = crate::theme::current();
    let _ = traces; // surfaced via the unified log + the Traces dialog
    let handle = drag_handle(border, cx);
    let header = header_row(state, fg, dim, theme.errors.severe.rgba(), cx);
    let controls = controls_row(fg, dim, border, accent, cx);
    let log = log_column(state, dim);

    div()
        .flex()
        .flex_col()
        .h(state.height)
        .flex_shrink_0()
        .border_t_1()
        .border_color(border)
        .bg(panel)
        .child(handle)
        .child(header)
        .child(controls)
        .child(log)
        .into_any_element()
}

/// 6px-tall strip at the very top of the dock that acts as the
/// drag handle for resizing. Hover changes the cursor to a
/// row-resize hint; mouse-down stashes the anchor. Move + up
/// listeners live on the Shell root (see `lib.rs` render) so
/// the pointer can travel anywhere during a drag without
/// leaving this 6px hit zone — a flick that exceeded 6px/event
/// otherwise dropped the gesture.
fn drag_handle(
    border: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id("debug-dock-handle")
        .h(px(DOCK_HANDLE_PX))
        .w_full()
        .border_b_1()
        .border_color(border)
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, ev: &gpui::MouseDownEvent, _w, cx| {
                shell.start_dock_resize(ev.position.y, cx);
            }),
        )
}

fn header_row(
    state: &DebugDockState,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    severe: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let frida = match state.agent_version.as_deref() {
        Some(v) => format!("frida {v}"),
        None => "frida".to_string(),
    };
    let device_label = state
        .device
        .model
        .clone()
        .unwrap_or_else(|| state.device.id.serial.clone());
    let title = format!("{} · {} · {}", state.package, device_label, frida);
    let _ = severe;
    div()
        .h(px(28.))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .px_3()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_sm()
                .text_color(fg)
                .font_family("Courier New")
                .child(SharedString::from(title)),
        )
        .child(
            div()
                .id("debug-dock-traces-btn")
                .text_xs()
                .text_color(dim)
                .cursor_pointer()
                .hover(|s| s.text_color(crate::theme::current().shell.accent.rgba()))
                .child(SharedString::from("Traces…"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|shell, _ev, _w, cx| {
                        shell.toggle_traces_dialog(cx);
                    }),
                ),
        )
        .child(
            div()
                .id("debug-dock-hooks-btn")
                .text_xs()
                .text_color(dim)
                .cursor_pointer()
                .hover(|s| s.text_color(crate::theme::current().shell.accent.rgba()))
                .child(SharedString::from("Hooks…"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|shell, _ev, _w, cx| {
                        shell.toggle_hooks_dialog(cx);
                    }),
                ),
        )
        .child(
            div()
                .id("debug-dock-copy-btn")
                .text_xs()
                .text_color(dim)
                .cursor_pointer()
                .hover(|s| s.text_color(crate::theme::current().shell.accent.rgba()))
                .child(SharedString::from("Copy"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|shell, _ev, _w, cx| {
                        shell.copy_debug_dock_log(cx);
                    }),
                ),
        )
        .child(
            div()
                .id("debug-dock-disconnect")
                .text_xs()
                .text_color(dim)
                .cursor_pointer()
                .hover(|s| s.text_color(crate::theme::current().errors.severe.rgba()))
                .child(SharedString::from("✕ Disconnect"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|shell, _ev, _w, cx| {
                        shell.close_debug_dock(cx);
                    }),
                ),
        )
}

fn controls_row(
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let _ = dim;
    // Restart — force-stops the app, relaunches it, waits
    // for the gadget to come back up, re-installs every
    // active trace + hook, then resumes. That gives the
    // user a one-click "rerun with my hooks in place" loop.
    // With no traces / hooks defined the orchestrator
    // collapses to "force-stop + start + resume," so the
    // button is also the right thing for plain restarts.
    let play = div()
        .id("debug-dock-restart")
        .px_3()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(accent)
        .text_sm()
        .text_color(fg)
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::current().hovers.standard.rgba()))
        .child(SharedString::from("↻ Restart"))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                shell.debug_restart(cx);
            }),
        );
    let stop = div()
        .id("debug-dock-stop")
        .px_3()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .text_sm()
        .text_color(fg)
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::current().hovers.standard.rgba()))
        .child(SharedString::from("◼ Stop"))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                shell.debug_stop(cx);
            }),
        );
    // M3.4 smoke test — loads a `send(1+1)` script via the
    // Frida session and waits for the resulting message in
    // the log. Useful to confirm the Frida wiring is live
    // before any feature code goes on top. Remove once
    // method tracing has its own UI surface.
    let smoke = div()
        .id("debug-dock-smoke")
        .px_3()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .text_sm()
        .text_color(dim)
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::current().hovers.standard.rgba()))
        .child(SharedString::from("⚡ Smoke"))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                shell.debug_smoke_test(cx);
            }),
        );
    div()
        .h(px(36.))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .px_3()
        .gap_2()
        .child(play)
        .child(stop)
        .child(smoke)
}

fn log_column(state: &DebugDockState, dim: gpui::Rgba) -> gpui::Stateful<gpui::Div> {
    // Newest at the bottom, scroll up to see older lines. Using
    // `overflow_y_scroll` so the log doesn't push the controls
    // out of view when it grows.
    let mut col = div()
        .id("debug-dock-log")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .px_3()
        .py_1()
        .flex()
        .flex_col()
        .gap_0p5();
    for line in &state.log {
        col = col.child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(SharedString::from(line.clone())),
        );
    }
    col
}
