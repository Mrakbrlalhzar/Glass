//! Right-side annotations pane.
//!
//! Lists every annotation in the loaded bundle, grouped by
//! artifact then by key kind. Each row is clickable — for
//! address-keyed entries it opens the listing at that address; for
//! class- and method-keyed entries it opens the smali tab; for
//! symbol-keyed entries it resolves the name through the symbol
//! map and opens the listing.
//!
//! The pane is fixed-width (280px) and lives to the right of the
//! tab body. Visibility is controlled by `shell.annotations_pane_open`
//! which is persisted in the BundleRecord.

use std::sync::Arc;

use gpui::{div, list, prelude::*, px, rgb, App, Context, ListAlignment, ListState, SharedString};

use glass_db::{AnnotationKey, ArtifactId};

use crate::palette::COLOUR_COMMENT;
use crate::{LoadedBundle, Shell};

pub const PANE_WIDTH: f32 = 280.;

#[derive(Clone)]
enum PaneRow {
    /// Artifact group header — short hash + label.
    ArtifactHeader { label: SharedString },
    /// One annotation entry.
    Entry {
        artifact: ArtifactId,
        key: AnnotationKey,
        primary: SharedString,
        facets: SharedString,
        dot_colour: Option<u32>,
    },
}

pub fn render_annotations_pane(
    // `shell` is unused today but will carry mutable input state once
    // Phase 4 lands inline rename / comment editing through the pane.
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
        let state = ListState::new(len, ListAlignment::Top, px(800.));
        let self_handle = cx.entity().downgrade();
        list(state, {
            let rows = rows.clone();
            move |index, _w, _cx| {
                let row = rows[index].clone();
                let handle = self_handle.clone();
                match row {
                    PaneRow::ArtifactHeader { label } => div()
                        .h(px(22.))
                        .px_3()
                        .pt_2()
                        .text_xs()
                        .text_color(rgb(0x808088))
                        .child(label)
                        .into_any_element(),
                    PaneRow::Entry {
                        artifact,
                        key,
                        primary,
                        facets,
                        dot_colour,
                    } => {
                        let dot: gpui::Background = match dot_colour {
                            Some(c) => gpui::rgba(c).into(),
                            None => rgb(0x4f7cff).into(),
                        };
                        div()
                            .id(("annot-row", index))
                            .h(px(36.))
                            .px_3()
                            .py_1()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .hover(|this| this.bg(rgb(0x2e2e34)))
                            .child(
                                div()
                                    .w(px(6.))
                                    .h(px(6.))
                                    .rounded_full()
                                    .flex_shrink_0()
                                    .bg(dot),
                            )
                            .child(
                                // Two-line label column. `overflow_x_scroll`
                                // lets long renames / comments scroll
                                // horizontally inside the pane rather
                                // than truncating; `whitespace_nowrap`
                                // on the text children keeps each line
                                // on one row.
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .id(("annot-row-inner", index))
                                    .overflow_x_scroll()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0xd6d6d6))
                                            .whitespace_nowrap()
                                            .child(primary),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(COLOUR_COMMENT))
                                            .whitespace_nowrap()
                                            .child(facets),
                                    ),
                            )
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_ev, _w, cx: &mut App| {
                                    let Some(entity) = handle.upgrade() else {
                                        return;
                                    };
                                    let artifact = artifact.clone();
                                    let key = key.clone();
                                    cx.update_entity(&entity, |shell, cx| {
                                        shell.navigate_to_annotation(artifact, key, cx);
                                    });
                                },
                            )
                            .into_any_element()
                    }
                }
            }
        })
        .h_full()
        .into_any_element()
    };

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
        .child(div().flex_1().child(body))
        .into_any_element()
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
        // Best-effort short label: 8-char hex prefix of the
        // artifact id. We could thread the artifact's display
        // label through but the prefix is unambiguous.
        let label = format!("{}", aid);
        let short = label.chars().take(10).collect::<String>();
        out.push(PaneRow::ArtifactHeader {
            label: SharedString::from(format!("Artifact {short}")),
        });
        // Order entries: addresses ascending, then symbols / classes
        // / methods alphabetically.
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
            // Compact, scannable: just the method short name + line.
            // Pulling out the class is harmless redundancy when the
            // pane is grouped by artifact already.
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
    // Only the comment goes into the secondary line — the dot on
    // the left already encodes the colour, and the primary line
    // shows the rename. Wide rows get a horizontal scrollbar.
    v.comment.clone().unwrap_or_default()
}
