//! Right-side annotations pane.
//!
//! Lists every annotation in the loaded bundle, grouped by
//! artifact. Each row is clickable — for address-keyed entries
//! it opens the listing at that address; for class- / method- /
//! method-line keys it opens the smali tab (and scrolls to the
//! specific line for MethodLine); symbol-keyed entries resolve
//! the name through the symbol map and open the listing.
//!
//! The pane is fixed-width (280px) and lives to the right of the
//! tab body. Visibility is controlled by `shell.annotations_pane_open`
//! which is persisted in the BundleRecord.
//!
//! Layout: each row is a fixed-height outer container with three
//! parts — a coloured dot at the left, a horizontally-scrolling
//! label column in the middle, and a delete icon pinned to the
//! right edge. Bottom border draws a ruled divider. Long labels
//! shift under the row via a per-pane `h_offset` (managed on
//! Shell) with a scrollbar across the bottom.

use std::sync::Arc;

use gpui::{
    div, list, prelude::*, px, rgb, App, Context, ListAlignment, ListState, Pixels, SharedString,
};

use glass_db::{AnnotationKey, ArtifactId};

use crate::palette::COLOUR_COMMENT;
use crate::scrollbar::horizontal_scrollbar_offset;
use crate::{LoadedBundle, Shell};

pub const PANE_WIDTH: f32 = 280.;
/// Logical width of the scrolling inner content area. Long labels
/// (full JNI strings, multi-clause comments) can extend up to
/// this. The scrollbar shows a thumb proportional to the visible
/// pane width over this content width.
const PANE_CONTENT_WIDTH: f32 = 900.;
/// Reserved at the right of each row for the delete icon. Sits on
/// the *outer* clipped row so the icon stays put under horizontal
/// scroll.
const PANE_DELETE_GUTTER: f32 = 28.;
const PANE_ROW_HEIGHT: f32 = 40.;
const PANE_HEADER_HEIGHT: f32 = 22.;

#[derive(Clone)]
enum PaneRow {
    ArtifactHeader { label: SharedString },
    Entry {
        artifact: ArtifactId,
        key: AnnotationKey,
        primary: SharedString,
        facets: SharedString,
        dot_colour: Option<u32>,
    },
}

pub fn render_annotations_pane(
    _shell: &mut Shell,
    bundle: &LoadedBundle,
    cx: &mut Context<Shell>,
    panel: gpui::Rgba,
    border: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
) -> gpui::AnyElement {
    let rows = build_rows(bundle);
    let header = div()
        .h(px(28.))
        .flex_shrink_0()
        .px_3()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(border)
        .bg(panel)
        .text_sm()
        .child(div().flex_1().text_color(fg).child("Annotations"))
        .child(
            div()
                .id("annotations-close")
                .text_color(dim)
                .cursor_pointer()
                .child("×")
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _ev, _w, cx| {
                        this.close_annotations_pane(cx);
                    }),
                ),
        );

    let h_offset = cx.entity().read(cx).annotations_pane_h_offset;

    let body: gpui::AnyElement = if rows.is_empty() {
        div()
            .flex_1()
            .p_3()
            .text_sm()
            .text_color(dim)
            .child(
                "No annotations yet. Right-click any row to add one, or use \
                 `glass set-rename` / `set-comment` / `set-colour` from the CLI.",
            )
            .into_any_element()
    } else {
        let rows: Arc<[PaneRow]> = rows.into();
        let len = rows.len();
        let state = ListState::new(len, ListAlignment::Top, px(2000.));
        let self_handle = cx.entity().downgrade();
        list(state, {
            let rows = rows.clone();
            move |index, _w, _cx| {
                render_pane_row(&rows[index], index, h_offset, self_handle.clone(), border)
            }
        })
        .h_full()
        .into_any_element()
    };

    // Horizontal scrollbar across the bottom — mirrors the pattern
    // used by the listing / smali viewers. Wraps the body in a
    // wheel handler so trackpad / horizontal scroll wheel updates
    // `h_offset` on Shell.
    let max_h = px(PANE_CONTENT_WIDTH) - px(PANE_WIDTH);
    let h_scrollbar = horizontal_scrollbar_offset(h_offset, px(PANE_CONTENT_WIDTH), border, dim);

    let body_with_wheel = div()
        .flex_1()
        .overflow_hidden()
        .on_scroll_wheel(cx.listener(
            move |this, ev: &gpui::ScrollWheelEvent, _w, cx| {
                let dx = ev.delta.pixel_delta(px(PANE_ROW_HEIGHT)).x;
                if dx != px(0.) {
                    this.scroll_annotations_pane_h(-dx, max_h, cx);
                }
            },
        ))
        .child(body);

    div()
        .w(px(PANE_WIDTH))
        .h_full()
        .flex_shrink_0()
        .border_l_1()
        .border_color(border)
        .bg(panel)
        .flex()
        .flex_col()
        .child(header)
        .child(body_with_wheel)
        .child(h_scrollbar)
        .into_any_element()
}

fn render_pane_row(
    row: &PaneRow,
    index: usize,
    h_offset: Pixels,
    handle: gpui::WeakEntity<Shell>,
    border: gpui::Rgba,
) -> gpui::AnyElement {
    match row {
        PaneRow::ArtifactHeader { label } => div()
            .h(px(PANE_HEADER_HEIGHT))
            .px_3()
            .pt_2()
            .text_xs()
            .text_color(rgb(0x808088))
            .child(label.clone())
            .into_any_element(),
        PaneRow::Entry {
            artifact,
            key,
            primary,
            facets,
            dot_colour,
        } => {
            let dot: gpui::Background = match dot_colour {
                Some(c) => gpui::rgba(*c).into(),
                None => rgb(0x4f7cff).into(),
            };

            // Inner scrolling content area: dot + two-line text
            // column. Sized at PANE_CONTENT_WIDTH and absolute-
            // positioned with `left(-h_offset)` so it slides under
            // the clipped outer row.
            let inner = div()
                .absolute()
                .top_0()
                .left(-h_offset)
                .h(px(PANE_ROW_HEIGHT))
                .w(px(PANE_CONTENT_WIDTH))
                .px_3()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded_full()
                        .flex_shrink_0()
                        .bg(dot),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(0xd6d6d6))
                                .whitespace_nowrap()
                                .child(primary.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(COLOUR_COMMENT))
                                .whitespace_nowrap()
                                .child(facets.clone()),
                        ),
                );

            // Outer clipped row: holds the scrolling inner +
            // pinned delete icon + bottom divider.
            let nav_handle = handle.clone();
            let del_handle = handle;
            let nav_artifact = artifact.clone();
            let nav_key = key.clone();
            let del_artifact = artifact.clone();
            let del_key = key.clone();

            let delete_icon = div()
                .id(("annot-delete", index))
                .absolute()
                .top_0()
                .right_0()
                .h(px(PANE_ROW_HEIGHT))
                .w(px(PANE_DELETE_GUTTER))
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(rgb(0x808088))
                .cursor_pointer()
                .hover(|this| this.text_color(rgb(0xff8080)).bg(rgb(0x2e2e34)))
                .child("×")
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |_ev, _w, cx: &mut App| {
                        cx.stop_propagation();
                        if let Some(entity) = del_handle.upgrade() {
                            let artifact = del_artifact.clone();
                            let key = del_key.clone();
                            cx.update_entity(&entity, |shell, cx| {
                                shell.clear_annotation_at(artifact, key, cx);
                            });
                        }
                    },
                );

            div()
                .id(("annot-row", index))
                .h(px(PANE_ROW_HEIGHT))
                .w_full()
                .relative()
                .overflow_hidden()
                .border_b_1()
                .border_color(border)
                .cursor_pointer()
                .hover(|this| this.bg(rgb(0x2e2e34)))
                .child(inner)
                .child(delete_icon)
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |_ev, _w, cx: &mut App| {
                        let Some(entity) = nav_handle.upgrade() else { return };
                        let artifact = nav_artifact.clone();
                        let key = nav_key.clone();
                        cx.update_entity(&entity, |shell, cx| {
                            shell.navigate_to_annotation(artifact, key, cx);
                        });
                    },
                )
                .into_any_element()
        }
    }
}

fn build_rows(bundle: &LoadedBundle) -> Vec<PaneRow> {
    let mut out: Vec<PaneRow> = Vec::new();
    for aid in bundle.artifact_ids.iter() {
        let Some(idx) = bundle.annotations.get(aid) else {
            continue;
        };
        if idx.is_empty() {
            continue;
        }
        let label = format!("{}", aid);
        let short = label.chars().take(10).collect::<String>();
        out.push(PaneRow::ArtifactHeader {
            label: SharedString::from(format!("Artifact {short}")),
        });
        let mut entries: Vec<_> = idx.iter().collect();
        entries.sort_by(|(a, _), (b, _)| sort_key(a).cmp(&sort_key(b)));
        for (k, v) in entries {
            let primary = primary_label(&k, v.rename.as_deref());
            let facets = facet_label(v);
            out.push(PaneRow::Entry {
                artifact: aid.clone(),
                key: k,
                primary: SharedString::from(primary),
                facets: SharedString::from(facets),
                dot_colour: v.colour,
            });
        }
    }
    out
}

fn sort_key(k: &AnnotationKey) -> (u8, String) {
    match k {
        AnnotationKey::Address(a) => (0, format!("{a:016x}")),
        AnnotationKey::Symbol(s) => (1, s.clone()),
        AnnotationKey::Class(c) => (2, c.clone()),
        AnnotationKey::Method(c, m) => (3, format!("{c}->{m}")),
        AnnotationKey::MethodLine(c, m, line) => (4, format!("{c}->{m}#{line:08}")),
    }
}

fn primary_label(k: &AnnotationKey, rename: Option<&str>) -> String {
    let raw = match k {
        AnnotationKey::Address(a) => format!("0x{a:x}"),
        AnnotationKey::Symbol(s) => s.clone(),
        AnnotationKey::Class(c) => c.clone(),
        AnnotationKey::Method(c, m) => format!("{c}->{m}"),
        AnnotationKey::MethodLine(c, m, line) => {
            let short = m.split('(').next().unwrap_or(m);
            let cls = c
                .trim_start_matches('L')
                .trim_end_matches(';')
                .rsplit('/')
                .next()
                .unwrap_or(c);
            if *line == 0 {
                format!("{cls}.{short}")
            } else {
                format!("{cls}.{short}:{line}")
            }
        }
    };
    match rename {
        Some(n) if !n.is_empty() => format!("{n}  ({raw})"),
        _ => raw,
    }
}

fn facet_label(v: &glass_db::Annotation) -> String {
    v.comment.clone().unwrap_or_default()
}
