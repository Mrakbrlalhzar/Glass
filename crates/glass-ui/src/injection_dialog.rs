//! Frida gadget-injection dialog.
//!
//! Modal overlay shown when the user clicks "Inject Frida
//! gadget" from the device picker dropdown. Renders the
//! `InjectionPlan` from `glass_frida` — package name, ABIs,
//! patch target, warnings — and offers two actions: Cancel
//! and Inject. M3.2b ships the dialog with a stub Inject; the
//! actual smali patch + APK rewrite + sign + adb install
//! pipeline lands in M3.2c.
//!
//! Layout mirrors the changes dialog: a 720px panel centred
//! near the top of the window with a dimming backdrop. Click
//! outside cancels.

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString};

use crate::{InjectionDialogState, InjectionPhase, InjectionProgress, Shell};

pub fn render_injection_dialog(
    shell: &Shell,
    state: &InjectionDialogState,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let _ = shell;
    let plan = &state.plan;

    let header = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_lg()
                .text_color(fg)
                .child(SharedString::from("Inject Frida gadget")),
        )
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(SharedString::from(
                    plan.package_name
                        .clone()
                        .unwrap_or_else(|| "(unknown package)".to_string()),
                )),
        );

    let abis_row = row_kv(
        "ABIs",
        if plan.abis.is_empty() {
            "(none — see warnings)".to_string()
        } else {
            plan.abis.join(", ")
        },
        dim,
        fg,
    );

    let patch_row = patch_target_row(plan, dim, fg);
    let gadget_row = row_kv(
        "Gadget",
        format!(
            "libfrida-gadget.so ({} bytes bundled)",
            glass_frida::for_android_abi("arm64-v8a")
                .map(|g| g.bytes.len())
                .unwrap_or(0)
        ),
        dim,
        fg,
    );

    let warnings_section = warnings_section(plan, dim);

    let footer = footer_row(state, fg, dim, border, accent, cx);

    let card = div()
        .id("injection-dialog-card")
        .w(px(720.))
        .bg(panel)
        .border_1()
        .border_color(border)
        .rounded_md()
        .shadow_lg()
        .p_5()
        .flex()
        .flex_col()
        .gap_4()
        .occlude()
        .child(header)
        .child(abis_row)
        .child(patch_row)
        .child(gadget_row)
        .child(warnings_section)
        .child(footer)
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
                shell.close_injection_dialog(cx);
            }),
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}

/// Single-row key-value display used for ABIs / patch / gadget
/// summary lines. Left column is the label, right column the
/// value.
fn row_kv(key: &'static str, value: String, dim: gpui::Rgba, fg: gpui::Rgba) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .gap_3()
        .items_baseline()
        .child(
            div()
                .w(px(110.))
                .flex_shrink_0()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(key)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_sm()
                .text_color(fg)
                .font_family("Courier New")
                .child(SharedString::from(value)),
        )
}

fn patch_target_row(
    plan: &glass_frida::InjectionPlan,
    dim: gpui::Rgba,
    fg: gpui::Rgba,
) -> gpui::Div {
    let (class, method, kind): (String, &str, &str) = match &plan.patch_target {
        glass_frida::PatchTarget::ExistingApplication {
            class_display, method, ..
        } => (
            class_display.clone(),
            method_label(*method),
            "Application class",
        ),
        glass_frida::PatchTarget::LauncherActivity {
            class_display, method, ..
        } => (
            class_display.clone(),
            method_label(*method),
            "Launcher activity",
        ),
        glass_frida::PatchTarget::SynthesiseRequired => (
            "(none — Glass would need to synthesise an Application class)".to_string(),
            "",
            "Synthesise required",
        ),
    };
    let value = if method.is_empty() {
        class.clone()
    } else {
        format!("{class}.{method}")
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(row_kv("Patch target", value, dim, fg))
        .child(
            div()
                .ml(px(122.))
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(format!(
                    "{kind} — Glass will insert `invoke-static …Ljava/lang/System;->loadLibrary(\"frida-gadget\")`"
                ))),
        )
}

fn method_label(m: glass_frida::PatchMethod) -> &'static str {
    match m {
        glass_frida::PatchMethod::ClassInit => "<clinit>",
        glass_frida::PatchMethod::OnCreate => "onCreate",
    }
}

fn warnings_section(plan: &glass_frida::InjectionPlan, dim: gpui::Rgba) -> gpui::Div {
    let mut col = div().flex().flex_col().gap_1();
    if plan.warnings.is_empty() {
        return col.child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from("No warnings.")),
        );
    }
    let theme = crate::theme::current();
    let amber = {
        let h = theme.errors.highlight.rgba();
        gpui::Rgba { r: h.r, g: h.g, b: h.b, a: 1.0 }
    };
    col = col.child(
        div()
            .text_xs()
            .text_color(dim)
            .child(SharedString::from("Warnings")),
    );
    for w in &plan.warnings {
        col = col.child(
            div()
                .text_xs()
                .text_color(amber)
                .child(SharedString::from(format_warning(w))),
        );
    }
    col
}

fn format_warning(w: &glass_frida::PlanWarning) -> String {
    use glass_frida::PlanWarning as W;
    match w {
        W::NoNativeLibsDir => {
            "APK has no native-libs directory — Glass will create one at lib/arm64-v8a/.".into()
        }
        W::NoEntryPoint => {
            "Manifest has no Application class and no MAIN activity — \
             Glass would need to synthesise one (not implemented yet).".into()
        }
        W::PatchClassNotLifted { class_jni } => {
            format!(
                "Manifest references {class_jni} but Glass didn't lift it. \
                 Double-check the patch target before injecting."
            )
        }
        W::UnusualApplicationParent { parent_jni } => {
            format!(
                "Application class extends an unfamiliar parent {parent_jni}. \
                 Static init should still be safe but worth checking."
            )
        }
        W::GadgetAlreadyPresent { abis } => {
            format!(
                "APK already contains a libfrida-gadget.so in: {}. \
                 Glass will overwrite.",
                abis.join(", "),
            )
        }
    }
}

fn footer_row(
    state: &InjectionDialogState,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    border: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let inject_enabled = !matches!(
        state.plan.patch_target,
        glass_frida::PatchTarget::SynthesiseRequired
    );
    let target_chip = state
        .target_device
        .as_ref()
        .map(|d| {
            format!(
                "Will install on: {}",
                d.model.clone().unwrap_or_else(|| d.id.serial.clone())
            )
        })
        .unwrap_or_else(|| "No device selected — Inject only writes the patched APK".to_string());

    let cancel = div()
        .id("injection-dialog-cancel")
        .px_3()
        .py_1p5()
        .border_1()
        .border_color(border)
        .rounded_sm()
        .text_sm()
        .text_color(fg)
        .cursor_pointer()
        .child(SharedString::from("Cancel"))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _ev, _w, cx| {
                shell.close_injection_dialog(cx);
            }),
        );

    let inject = div()
        .id("injection-dialog-inject")
        .px_3()
        .py_1p5()
        .border_1()
        .border_color(if inject_enabled { border } else { border })
        .rounded_sm()
        .text_sm()
        .text_color(if inject_enabled { fg } else { dim })
        .child(SharedString::from("Inject"))
        .when(inject_enabled, |d| {
            d.cursor_pointer().on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|shell, _ev, _w, cx| {
                    shell.execute_injection(cx);
                }),
            )
        });

    // "Inject & Install" — only enabled when the selected
    // device is an authorised Android phone. The pipeline
    // exports → signs → installs in one shot.
    let install_enabled = inject_enabled
        && state
            .target_device
            .as_ref()
            .map(|d| {
                matches!(d.state, glass_device::AuthState::Authorised)
                    && matches!(
                        d.id.platform,
                        glass_device::DevicePlatform::Android
                    )
            })
            .unwrap_or(false);
    let install_label = match state.target_device.as_ref() {
        Some(d) if install_enabled => format!(
            "Inject & Install on {}",
            d.model.clone().unwrap_or_else(|| d.id.serial.clone())
        ),
        _ => "Inject & Install".to_string(),
    };
    let inject_and_install = div()
        .id("injection-dialog-inject-install")
        .px_3()
        .py_1p5()
        .border_1()
        .border_color(if install_enabled { accent } else { border })
        .rounded_sm()
        .text_sm()
        .text_color(if install_enabled { fg } else { dim })
        .child(SharedString::from(install_label))
        .when(install_enabled, |d| {
            d.cursor_pointer().on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|shell, _ev, _w, cx| {
                    shell.execute_injection_and_install(cx);
                }),
            )
        });

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(target_chip)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(cancel)
                .child(inject)
                .child(inject_and_install),
        )
}

/// Modal overlay shown while Inject & Install is running, and
/// after it finishes (until the user clicks Dismiss). Same
/// shape as the dialog: 720px panel centred near the top.
pub fn render_injection_progress(
    progress: &InjectionProgress,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    accent: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let running = progress.result.is_none();
    let theme = crate::theme::current();
    let phase_color = match (&progress.result, progress.phase) {
        (Some(Err(_)), _) => theme.errors.highlight.rgba(),
        (_, InjectionPhase::Done) => accent,
        _ => fg,
    };
    let mut log_col = div().flex().flex_col().gap_0p5();
    for line in &progress.log {
        log_col = log_col.child(
            div()
                .text_xs()
                .text_color(dim)
                .font_family("Courier New")
                .child(SharedString::from(line.clone())),
        );
    }
    let mut card = div()
        .id("injection-progress-card")
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
        .child(
            div()
                .text_lg()
                .text_color(phase_color)
                .child(SharedString::from(progress.phase.label())),
        )
        .child(log_col)
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        );
    if !running {
        let dismiss = div()
            .id("injection-progress-dismiss")
            .px_3()
            .py_1p5()
            .border_1()
            .border_color(accent)
            .rounded_sm()
            .text_sm()
            .text_color(fg)
            .cursor_pointer()
            .child(SharedString::from("Dismiss"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|shell, _ev, _w, cx| {
                    shell.dismiss_injection_progress(cx);
                }),
            );
        card = card.child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .child(dismiss),
        );
    }

    div()
        .absolute()
        .inset_0()
        .bg(theme.modals.overlay_light.rgba())
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                // Backdrop clicks don't dismiss — the user
                // sees the result via the explicit Dismiss
                // button. Avoids losing important error info
                // to a stray click.
                cx.stop_propagation();
            },
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}
