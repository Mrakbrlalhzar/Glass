//! Toolbar chip + dropdown for the cross-platform device
//! picker. Reads `Shell.device_snapshot` (populated by the
//! poll task in `app.rs`) and renders:
//!
//!   * A small chip in the header showing the selected device
//!     (or "No device").
//!   * When clicked, an overlay dropdown listing every device
//!     the snapshot contains, grouped by platform, plus a
//!     footer with backend status (ADB found / iOS reachable).
//!
//! Selection is stored on `Shell.selected_device`. The chip
//! tinting shows authorisation state — authorised devices use
//! the committed-bg green wash, unauthorised devices use the
//! errors-highlight tint so the user notices they need to act.

use gpui::{div, prelude::*, px, App, Context, SharedString};

use crate::Shell;

pub(crate) fn render_chip(
    shell: &Shell,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let theme = crate::theme::current();
    let border = theme.shell.border.rgba();
    let (label, secondary, state_tint) = match shell.selected_device.as_ref() {
        Some(id) => {
            let info = shell.device_snapshot.iter().find(|d| &d.id == id);
            let label = info
                .and_then(|i| i.model.clone())
                .unwrap_or_else(|| id.serial.clone());
            // Default tint comes from device auth state; the
            // Frida probe overlays a warning tint when the
            // server is unreachable.
            let mut tint = info
                .map(|i| match i.state {
                    glass_device::AuthState::Authorised => {
                        theme.state.committed_bg.rgba()
                    }
                    glass_device::AuthState::Unauthorised => {
                        let h = theme.errors.highlight.rgba();
                        gpui::Rgba { r: h.r, g: h.g, b: h.b, a: 0.22 }
                    }
                    glass_device::AuthState::Offline => {
                        theme.hovers.standard.rgba()
                    }
                })
                .unwrap_or(gpui::rgba(0x00000000));
            // Compose the secondary chip line from the Frida
            // probe cache. Until the poll task runs we show
            // "probing…"; on success "Frida <version>"; on
            // failure a short hint like "no Frida".
            let secondary = match shell.frida_probes.get(id) {
                None => "probing Frida…".to_string(),
                Some(entry) => match &entry.result {
                    Ok(report) => {
                        let label = match report.kind {
                            glass_frida::FridaKind::Server => "frida-server",
                            glass_frida::FridaKind::Gadget => "frida-gadget",
                        };
                        match report.agent_version.as_deref() {
                            Some(v) => format!("{label} {v}"),
                            None => label.to_string(),
                        }
                    }
                    Err(glass_frida::FridaError::NotBuilt) => {
                        "Frida disabled in this build".to_string()
                    }
                    Err(glass_frida::FridaError::ServerUnreachable) => {
                        // Soften the tint to amber so the chip
                        // reads as "needs attention" rather
                        // than the harder Unauthorised red.
                        let h = theme.errors.highlight.rgba();
                        tint =
                            gpui::Rgba { r: h.r, g: h.g, b: h.b, a: 0.14 };
                        "no Frida — inject gadget to enable".to_string()
                    }
                    Err(glass_frida::FridaError::DeviceNotFound(_)) => {
                        let h = theme.errors.highlight.rgba();
                        tint =
                            gpui::Rgba { r: h.r, g: h.g, b: h.b, a: 0.14 };
                        "Frida can't see this device".to_string()
                    }
                    Err(e) => format!("Frida: {e}"),
                },
            };
            (label, Some(secondary), tint)
        }
        None => ("No device".to_string(), None, gpui::rgba(0x00000000)),
    };
    let mut chip = div()
        .id("device-picker-chip")
        .px_3()
        .h(px(24.))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .rounded_sm()
        .text_sm()
        .text_color(fg)
        .border_1()
        .border_color(border)
        .bg(state_tint)
        .hover(move |s| s.bg(theme.hovers.standard.rgba()))
        .cursor_pointer()
        .child(SharedString::from(label));
    if let Some(s) = secondary {
        chip = chip.child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(s)),
        );
    }
    chip.child(
        div()
            .text_xs()
            .text_color(dim)
            .child(SharedString::from("▾")),
    )
    .on_mouse_down(
        gpui::MouseButton::Left,
        cx.listener(|this, _ev, _w, cx| {
            this.device_picker_open = !this.device_picker_open;
            cx.notify();
        }),
    )
}

pub(crate) fn render_dropdown(
    shell: &Shell,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::AnyElement {
    let mut card = div()
        .w(px(360.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_sm()
        .shadow_lg()
        .flex()
        .flex_col()
        .occlude()
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        );
    let mut last_platform: Option<glass_device::DevicePlatform> = None;
    if shell.device_snapshot.is_empty() {
        card = card.child(
            div()
                .px_3()
                .py_2()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(
                    "No devices detected. Plug in a USB phone / tablet.",
                )),
        );
    }
    for (idx, info) in shell.device_snapshot.iter().enumerate() {
        if last_platform != Some(info.id.platform) {
            card = card.child(platform_header(info.id.platform, dim, border));
            last_platform = Some(info.id.platform);
        }
        card = card.child(device_row(idx, info, shell, fg, dim, accent, cx));
    }
    if let Some(action) = inject_gadget_action(shell, fg, dim, border, accent, cx) {
        card = card.child(action);
    }
    if let Some(action) = connect_action(shell, fg, dim, border, accent, cx) {
        card = card.child(action);
    }
    card = card.child(footer(shell, dim, border));
    // Backdrop closes on outside-click. The chip itself is the
    // anchor; we render the dropdown absolutely positioned in
    // the top-right where the chip lives.
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                this.device_picker_open = false;
                cx.notify();
            }),
        )
        .child(
            div()
                .absolute()
                .top(px(36.))
                .right(px(12.))
                .child(card),
        )
        .into_any_element()
}

fn platform_header(
    platform: glass_device::DevicePlatform,
    dim: gpui::Rgba,
    border: gpui::Rgba,
) -> gpui::Div {
    div()
        .px_3()
        .py_1()
        .text_xs()
        .text_color(dim)
        .border_b_1()
        .border_color(border)
        .child(SharedString::from(platform.label()))
}

fn device_row(
    index: usize,
    info: &glass_device::DeviceInfo,
    shell: &Shell,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let theme = crate::theme::current();
    let id_for_click = info.id.clone();
    let is_selected = shell.selected_device.as_ref() == Some(&info.id);
    let bg = if is_selected {
        theme.modals.palette_selected.rgba()
    } else {
        gpui::rgba(0x00000000)
    };
    let primary = info
        .model
        .clone()
        .unwrap_or_else(|| info.id.serial.clone());
    let secondary = match info.state {
        glass_device::AuthState::Authorised => {
            info.os_version.clone().unwrap_or_default()
        }
        glass_device::AuthState::Unauthorised => {
            "(unauthorised — accept the prompt on the device)".into()
        }
        glass_device::AuthState::Offline => "(offline)".into(),
    };
    div()
        .id(("device-row", index))
        .px_3()
        .py_1()
        .bg(bg)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(move |s| s.bg(theme.modals.palette_hover.rgba()))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_sm()
                .text_color(fg)
                .child(SharedString::from(primary)),
        )
        .child(
            div()
                .text_xs()
                .text_color(if is_selected { accent } else { dim })
                .child(SharedString::from(secondary)),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |shell, _ev, _w, cx| {
                let id = id_for_click.clone();
                // Drop any cached probe for the device we're
                // selecting — when the user picks something
                // from the dropdown they want a fresh read,
                // not whatever was there from a previous
                // session. The poll task notices the missing
                // entry on its next tick and probes.
                shell.frida_probes.remove(&id);
                shell.selected_device = Some(id);
                shell.device_picker_open = false;
                cx.notify();
            }),
        )
}

/// "Inject Frida gadget into this bundle" affordance — only
/// rendered when the selected device's Frida probe says no
/// server is reachable AND the current bundle is an APK (the
/// only thing the injector knows how to patch today).
fn inject_gadget_action(
    shell: &Shell,
    _fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> Option<gpui::Stateful<gpui::Div>> {
    let selected = shell.selected_device.as_ref()?;
    let probe = shell.frida_probes.get(selected)?;
    // Only show when we know there's no Frida — don't tempt
    // the user to re-inject a device that's already happy.
    if !matches!(
        probe.result,
        Err(glass_frida::FridaError::ServerUnreachable)
    ) {
        return None;
    }
    let bundle = shell.bundle()?;
    bundle.android_manifest.as_ref()?;
    Some(
        div()
            .id("inject-gadget-action")
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(border)
            .flex()
            .flex_col()
            .gap_1()
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::current().modals.palette_hover.rgba()))
            .child(
                div()
                    .text_sm()
                    .text_color(accent)
                    .child(SharedString::from("Inject Frida gadget into this APK…")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(dim)
                    .child(SharedString::from(
                        "Adds libfrida-gadget.so and patches the Application class to load it.",
                    )),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|shell, _ev, _w, cx| {
                    shell.open_injection_dialog(cx);
                }),
            ),
    )
}

/// "Connect" affordance — opens the bottom debug dock for the
/// loaded APK on the selected device. Only renders when:
///   * A device is selected and its Frida probe says
///     Server or Gadget (i.e. we can actually talk to the
///     app once it's running).
///   * The loaded bundle is an APK with a manifest (so we
///     have a package name to control).
///   * The dock isn't already open (no point offering it).
fn connect_action(
    shell: &Shell,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> Option<gpui::Stateful<gpui::Div>> {
    if shell.debug_dock.is_some() {
        return None;
    }
    let selected = shell.selected_device.as_ref()?;
    let probe = shell.frida_probes.get(selected)?;
    let report = probe.result.as_ref().ok()?;
    if !matches!(
        report.kind,
        glass_frida::FridaKind::Server | glass_frida::FridaKind::Gadget,
    ) {
        return None;
    }
    let bundle = shell.bundle()?;
    bundle.android_manifest.as_ref()?;
    let _ = (fg, accent);
    Some(
        div()
            .id("connect-debug-action")
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(border)
            .flex()
            .flex_col()
            .gap_1()
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::current().modals.palette_hover.rgba()))
            .child(
                div()
                    .text_sm()
                    .text_color(accent)
                    .child(SharedString::from("Connect debugger…")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(dim)
                    .child(SharedString::from(
                        "Open the debug dock to launch / stop the app on this device.",
                    )),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|shell, _ev, _w, cx| {
                    shell.open_debug_dock(cx);
                }),
            ),
    )
}

fn footer(shell: &Shell, dim: gpui::Rgba, border: gpui::Rgba) -> gpui::Div {
    let adb_line = match &shell.device_backend_status.adb {
        Ok(info) => format!(
            "ADB: {} ({})",
            info.binary_path.display(),
            info.version
        ),
        Err(e) => format!("ADB: {e}"),
    };
    let ios_line = match &shell.device_backend_status.ios {
        Ok(info) => {
            if info.usbmuxd_reachable {
                "iOS: usbmuxd reachable".to_string()
            } else {
                "iOS: usbmuxd not reachable".to_string()
            }
        }
        Err(e) => format!("iOS: {e}"),
    };
    let frida_line = if glass_frida::FridaRuntime::enabled() {
        "Frida: built-in".to_string()
    } else {
        "Frida: support not built (rebuild with `--features frida`)".to_string()
    };
    div()
        .px_3()
        .py_1p5()
        .border_t_1()
        .border_color(border)
        .flex()
        .flex_col()
        .gap_0p5()
        .text_xs()
        .text_color(dim)
        .child(SharedString::from(adb_line))
        .child(SharedString::from(ios_line))
        .child(SharedString::from(frida_line))
}
