//! About-Glass modal overlay.
//!
//! Triggered by Glass → About Glass in the macOS menu. Renders a
//! centered card with build metadata, repo link, licence, and
//! third-party attributions. Click outside or press Escape to
//! dismiss.

use gpui::{div, prelude::*, px, rgb, AnyElement, App, Context, SharedString};

use crate::Shell;

/// Compile-time build metadata, emitted by `build.rs`.
const BUILD_DATE: &str = env!("GLASS_BUILD_DATE");
const GIT_COMMIT: &str = env!("GLASS_GIT_COMMIT");
const GIT_DESCRIBE: &str = env!("GLASS_GIT_DESCRIBE");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO_URL: &str = "https://github.com/azw413/Glass";

/// Direct third-party dependencies that ship in the final binary.
/// Kept hand-curated rather than auto-derived from `cargo metadata`
/// because the curated list lets us annotate the licence + one-line
/// description per crate. Transitive deps are not listed.
const ATTRIBUTIONS: &[(&str, &str, &str)] = &[
    ("gpui", "Apache-2.0", "GPU-accelerated UI framework (Zed)"),
    ("smali", "GPL-3.0-only", "APK / DEX / smali parser"),
    ("armv8-encode", "MIT", "AArch64 disassembler"),
    ("redb", "MIT/Apache-2.0", "Embedded key-value store"),
    ("blake3", "Apache-2.0/CC0-1.0", "Content-addressed hashing"),
    ("rayon", "MIT/Apache-2.0", "Data-parallelism runtime"),
    ("anyhow", "MIT/Apache-2.0", "Error handling"),
    ("clap", "MIT/Apache-2.0", "CLI parsing"),
    ("tracing", "MIT", "Structured logging"),
    ("plist", "MIT", "Info.plist parsing"),
    ("serde", "MIT/Apache-2.0", "Serialization"),
    ("symbolic-demangle", "MIT", "C++/Rust/Swift demangling"),
    ("parking_lot", "MIT/Apache-2.0", "Locking primitives"),
];

pub fn render_about(
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let header = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_2xl()
                .text_color(fg)
                .child(SharedString::from("Glass")),
        )
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(
                    "A fast, native, mobile-app interactive disassembler.",
                )),
        );

    let describe = if GIT_DESCRIBE.is_empty() {
        format!("commit {GIT_COMMIT}")
    } else {
        format!("{GIT_DESCRIBE} ({GIT_COMMIT})")
    };
    let build_meta = div()
        .flex()
        .flex_col()
        .gap_1()
        .text_xs()
        .font_family("Menlo")
        .text_color(dim)
        .child(SharedString::from(format!("Version  {VERSION}")))
        .child(SharedString::from(format!("Build    {BUILD_DATE}")))
        .child(SharedString::from(describe));

    let links = div()
        .flex()
        .flex_col()
        .gap_1()
        .text_xs()
        .text_color(fg)
        .child(SharedString::from(format!("Source   {REPO_URL}")))
        .child(SharedString::from("Licence  GPL-3.0-only (inherits from smali)"));

    let mut attrib_list = div()
        .flex()
        .flex_col()
        .gap_1()
        .text_xs()
        .font_family("Menlo")
        .text_color(dim);
    attrib_list = attrib_list.child(
        div()
            .text_color(fg)
            .child(SharedString::from("Third-party (direct dependencies)")),
    );
    for (name, licence, blurb) in ATTRIBUTIONS {
        attrib_list = attrib_list.child(
            div()
                .flex()
                .flex_row()
                .gap_3()
                .child(div().w(px(160.)).child(SharedString::from(*name)))
                .child(
                    div()
                        .w(px(160.))
                        .text_color(rgb(0x808088))
                        .child(SharedString::from(*licence)),
                )
                .child(div().child(SharedString::from(*blurb))),
        );
    }

    let card = div()
        .id("about-card")
        .w(px(620.))
        .max_h(px(640.))
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
        .child(build_meta)
        .child(links)
        .child(attrib_list)
        // Eat clicks inside so the backdrop dismiss handler doesn't
        // fire when the user clicks on the card content.
        .on_mouse_down(
            gpui::MouseButton::Left,
            |_ev, _w, cx: &mut App| {
                cx.stop_propagation();
            },
        );

    div()
        .absolute()
        .inset_0()
        .bg(gpui::rgba(0x000000bb))
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                this.close_about(cx);
            }),
        )
        .child(div().mt(px(80.)).child(card))
        .into_any_element()
}
