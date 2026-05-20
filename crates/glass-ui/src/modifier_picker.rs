// All API here is consumed by M1.3 (class-decl popover) and beyond.
// Allow dead_code until at least one popover wires it in.
#![allow(dead_code)]
//! Reusable modifier picker for class / field / method popovers.
//!
//! Renders the JVM access flags as a 3-way visibility radio
//! (Public / Protected / Private, with "none" = package-private)
//! plus a wrap of checkboxes for the remaining modifiers. The
//! valid set depends on where the modifier is being attached —
//! `synchronized` is method-only, `volatile` is field-only,
//! `interface` is class-only, etc. — and forbidden combinations
//! (e.g. `final` + `abstract`) are greyed out at render time so
//! the user is steered toward a valid choice rather than
//! erroring on save.
//!
//! Pure renderer: caller owns the `Vec<Modifier>` and updates it
//! inside the toggle callback. The widget itself is stateless,
//! same shape as `checkbox.rs`.

use gpui::{div, px, App, InteractiveElement, ParentElement, Rgba, SharedString, Styled};
use smali::types::Modifier;

use crate::checkbox::checkbox;

/// Which kind of declaration the modifiers are attached to.
/// Drives both which modifiers are *available* and which
/// combinations are *forbidden*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierSite {
    Class,
    Field,
    Method,
}

impl ModifierSite {
    /// Non-visibility modifiers offered for this site, in display
    /// order. Visibility (Public/Protected/Private) is handled
    /// separately as a radio. Order matters because the picker
    /// renders these as a wrap of checkboxes.
    pub fn allowed_checkboxes(self) -> &'static [Modifier] {
        match self {
            ModifierSite::Class => &[
                Modifier::Static,
                Modifier::Final,
                Modifier::Abstract,
                Modifier::Interface,
                Modifier::Annotation,
                Modifier::Enum,
                Modifier::Synthetic,
            ],
            ModifierSite::Field => &[
                Modifier::Static,
                Modifier::Final,
                Modifier::Volatile,
                Modifier::Transient,
                Modifier::Enum,
                Modifier::Synthetic,
            ],
            ModifierSite::Method => &[
                Modifier::Static,
                Modifier::Final,
                Modifier::Abstract,
                Modifier::Synchronized,
                Modifier::Native,
                Modifier::Strict,
                Modifier::Varargs,
                Modifier::Bridge,
                Modifier::Constructor,
                Modifier::Synthetic,
            ],
        }
    }
}

/// Visibility radio value. `None` is JVM package-private (no flag set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Package,
    Public,
    Protected,
    Private,
}

impl Visibility {
    /// Pull the visibility out of a modifier vec.
    pub fn from_modifiers(mods: &[Modifier]) -> Self {
        for m in mods {
            match m {
                Modifier::Public => return Visibility::Public,
                Modifier::Protected => return Visibility::Protected,
                Modifier::Private => return Visibility::Private,
                _ => {}
            }
        }
        Visibility::Package
    }

    /// The (at most one) Modifier this visibility maps to.
    pub fn to_modifier(self) -> Option<Modifier> {
        match self {
            Visibility::Package => None,
            Visibility::Public => Some(Modifier::Public),
            Visibility::Protected => Some(Modifier::Protected),
            Visibility::Private => Some(Modifier::Private),
        }
    }
}

/// Replace any existing visibility modifier in `mods` with the one
/// implied by `vis`. Returns the new vec; doesn't touch non-visibility
/// flags. Useful when the radio fires.
pub fn set_visibility(mods: &[Modifier], vis: Visibility) -> Vec<Modifier> {
    let mut out: Vec<Modifier> = mods
        .iter()
        .filter(|m| {
            !matches!(m, Modifier::Public | Modifier::Protected | Modifier::Private)
        })
        .cloned()
        .collect();
    if let Some(m) = vis.to_modifier() {
        // Visibility is conventionally first in smali declarations,
        // so prepend it. The DEX writer doesn't care about order;
        // this is purely cosmetic when the user re-views the line.
        out.insert(0, m);
    }
    out
}

/// Toggle a non-visibility modifier in `mods`. Idempotent:
/// inserting an existing one is a no-op, removing an absent one
/// is a no-op.
pub fn toggle_modifier(mods: &[Modifier], target: Modifier) -> Vec<Modifier> {
    if mods.iter().any(|m| m == &target) {
        mods.iter().filter(|m| *m != &target).cloned().collect()
    } else {
        let mut out: Vec<Modifier> = mods.to_vec();
        out.push(target);
        out
    }
}

/// Return `Some(reason)` if `target` cannot be combined with the
/// currently-set modifiers in `mods`, given `site`. Drives the
/// disabled state + hover tooltip on each checkbox.
///
/// Encodes the JVM access-flag rules we actually care about. Not
/// exhaustive — we cover the ones a user is likely to set by hand.
pub fn forbidden_reason(
    mods: &[Modifier],
    target: &Modifier,
    site: ModifierSite,
) -> Option<&'static str> {
    let has = |m: Modifier| mods.iter().any(|x| x == &m);
    let final_ = has(Modifier::Final);
    let abstract_ = has(Modifier::Abstract);
    let static_ = has(Modifier::Static);
    let native_ = has(Modifier::Native);
    let synchronized_ = has(Modifier::Synchronized);
    let strict_ = has(Modifier::Strict);
    let private_ = matches!(Visibility::from_modifiers(mods), Visibility::Private);
    match (site, target) {
        // final + abstract is contradictory at any site.
        (_, &Modifier::Final) if abstract_ => Some("Cannot be both final and abstract."),
        (_, &Modifier::Abstract) if final_ => Some("Cannot be both final and abstract."),

        // Fields: final + volatile is forbidden.
        (ModifierSite::Field, &Modifier::Final) if has(Modifier::Volatile) => {
            Some("A field cannot be both final and volatile.")
        }
        (ModifierSite::Field, &Modifier::Volatile) if final_ => {
            Some("A field cannot be both final and volatile.")
        }

        // Methods: abstract is incompatible with static / final /
        // native / synchronized / strict, and with private.
        (ModifierSite::Method, &Modifier::Static) if abstract_ => {
            Some("An abstract method cannot be static.")
        }
        (ModifierSite::Method, &Modifier::Native) if abstract_ => {
            Some("An abstract method cannot be native.")
        }
        (ModifierSite::Method, &Modifier::Synchronized) if abstract_ => {
            Some("An abstract method cannot be synchronized.")
        }
        (ModifierSite::Method, &Modifier::Strict) if abstract_ => {
            Some("An abstract method cannot be strictfp.")
        }
        (ModifierSite::Method, &Modifier::Abstract)
            if static_ || native_ || synchronized_ || strict_ || private_ =>
        {
            Some("Abstract methods can't be static / native / synchronized / strict / private.")
        }

        // Classes: an annotation type must also be an interface; we
        // surface that hint rather than enforcing it strictly here.
        (ModifierSite::Class, &Modifier::Annotation) if !has(Modifier::Interface) => {
            Some("Annotations are also interfaces — consider enabling Interface.")
        }

        _ => None,
    }
}

/// Render the picker. Width is up to the caller (typically wraps
/// inside a popover card).
///
/// `on_visibility` fires when the radio changes; the caller should
/// rebuild the modifier vec with `set_visibility`.
/// `on_toggle` fires when a checkbox is clicked; the caller should
/// rebuild the modifier vec with `toggle_modifier`.
#[allow(clippy::too_many_arguments)]
pub fn render_modifier_picker<FV, FT>(
    id_prefix: &'static str,
    site: ModifierSite,
    mods: &[Modifier],
    fg: Rgba,
    dim: Rgba,
    accent: Rgba,
    on_visibility: FV,
    on_toggle: FT,
) -> gpui::Div
where
    FV: Fn(Visibility, &mut App) + Clone + 'static,
    FT: Fn(Modifier, &mut App) + Clone + 'static,
{
    let current_vis = Visibility::from_modifiers(mods);
    let radio_row = render_visibility_radio(
        id_prefix,
        current_vis,
        fg,
        dim,
        accent,
        on_visibility,
    );

    let mut check_row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_x_4()
        .gap_y_1();
    for (i, m_ref) in site.allowed_checkboxes().iter().enumerate() {
        let m = m_ref.clone();
        let checked = mods.iter().any(|x| x == &m);
        let forbidden = forbidden_reason(mods, &m, site);
        let id: &'static str =
            Box::leak(format!("{id_prefix}-cb-{i}").into_boxed_str());
        // Modifier::to_str() borrows the variant, so own an owned
        // copy for the closure / disabled renderer.
        let label = SharedString::from(m.to_str().to_string());
        if let Some(reason) = forbidden {
            // Greyed-out variant: render label + box but no click
            // handler. Hover would show the reason as a tooltip
            // (wiring deferred — see render_disabled_checkbox).
            check_row =
                check_row.child(render_disabled_checkbox(id, label, checked, dim, reason));
        } else {
            let on_toggle = on_toggle.clone();
            let m_for_cb = m.clone();
            check_row = check_row.child(checkbox(
                id,
                label,
                checked,
                fg,
                dim,
                accent,
                move |cx| {
                    on_toggle(m_for_cb.clone(), cx);
                },
            ));
        }
    }

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(radio_row)
        .child(check_row)
}

/// 3-way visibility radio. Renders four pill buttons (Public,
/// Protected, Private, Package) — exactly one is highlighted.
#[allow(clippy::too_many_arguments)]
fn render_visibility_radio<F>(
    id_prefix: &'static str,
    current: Visibility,
    fg: Rgba,
    dim: Rgba,
    accent: Rgba,
    on_visibility: F,
) -> gpui::Div
where
    F: Fn(Visibility, &mut App) + Clone + 'static,
{
    let opts = [
        ("public", Visibility::Public),
        ("protected", Visibility::Protected),
        ("private", Visibility::Private),
        ("package", Visibility::Package),
    ];
    let mut row = div()
        .flex()
        .flex_row()
        .gap_2();
    for (i, (label, vis)) in opts.iter().copied().enumerate() {
        let id: &'static str =
            Box::leak(format!("{id_prefix}-vis-{i}").into_boxed_str());
        let selected = current == vis;
        let on_visibility = on_visibility.clone();
        row = row.child(
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .rounded_sm()
                .border_1()
                .border_color(if selected { accent } else { dim })
                .bg(if selected { accent } else { gpui::rgba(0x00000000) })
                .text_xs()
                .text_color(if selected { fg } else { dim })
                .cursor_pointer()
                .child(SharedString::from(label))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |_ev, _w, cx: &mut App| {
                        on_visibility(vis, cx);
                    },
                ),
        );
    }
    row
}

/// Greyed-out checkbox: same shape as `checkbox` but no click
/// handler and a tooltip explaining why it's disabled.
fn render_disabled_checkbox(
    _id: &'static str,
    label: SharedString,
    checked: bool,
    dim: Rgba,
    _reason: &'static str,
) -> gpui::Div {
    // Tooltip wiring needs an entity context we don't have at
    // render time here (the picker is built inline inside a parent
    // popover). For now we just render dimmed without a tooltip
    // and rely on the disabled visual signal; a follow-up can
    // route the reason through the parent's tooltip entity.
    let tick = if checked { "✓" } else { "" };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(12.))
                .h(px(12.))
                .border_1()
                .border_color(dim)
                .bg(gpui::rgba(0x00000000))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(tick)),
        )
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(label),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_round_trip_through_modifiers() {
        for vis in [
            Visibility::Public,
            Visibility::Protected,
            Visibility::Private,
            Visibility::Package,
        ] {
            let m = set_visibility(&[], vis);
            assert_eq!(Visibility::from_modifiers(&m), vis);
        }
    }

    #[test]
    fn set_visibility_replaces_existing() {
        let mods = vec![Modifier::Public, Modifier::Final, Modifier::Static];
        let updated = set_visibility(&mods, Visibility::Private);
        assert_eq!(Visibility::from_modifiers(&updated), Visibility::Private);
        // Final and Static survive the change.
        assert!(updated.iter().any(|m| m == &Modifier::Final));
        assert!(updated.iter().any(|m| m == &Modifier::Static));
        // Only one visibility flag at the end.
        let vis_count = updated
            .iter()
            .filter(|m| {
                matches!(m, Modifier::Public | Modifier::Protected | Modifier::Private)
            })
            .count();
        assert_eq!(vis_count, 1);
    }

    #[test]
    fn set_visibility_package_strips_all() {
        let mods = vec![Modifier::Protected, Modifier::Final];
        let updated = set_visibility(&mods, Visibility::Package);
        let vis_count = updated
            .iter()
            .filter(|m| {
                matches!(m, Modifier::Public | Modifier::Protected | Modifier::Private)
            })
            .count();
        assert_eq!(vis_count, 0);
        assert!(updated.iter().any(|m| m == &Modifier::Final));
    }

    #[test]
    fn toggle_modifier_is_idempotent() {
        let mods = vec![Modifier::Public];
        let added = toggle_modifier(&mods, Modifier::Final);
        assert!(added.iter().any(|m| m == &Modifier::Final));
        let removed = toggle_modifier(&added, Modifier::Final);
        assert!(!removed.iter().any(|m| m == &Modifier::Final));
        // Toggling twice gets us back to the original state.
        assert_eq!(removed.len(), mods.len());
    }

    #[test]
    fn allowed_modifiers_are_site_filtered() {
        // Sanity-check a few site-specific entries are where we
        // expect them, and not in the wrong place.
        let class = ModifierSite::Class.allowed_checkboxes();
        assert!(class.iter().any(|m| m == &Modifier::Interface));
        assert!(!class.iter().any(|m| m == &Modifier::Synchronized));

        let field = ModifierSite::Field.allowed_checkboxes();
        assert!(field.iter().any(|m| m == &Modifier::Volatile));
        assert!(!field.iter().any(|m| m == &Modifier::Abstract));

        let method = ModifierSite::Method.allowed_checkboxes();
        assert!(method.iter().any(|m| m == &Modifier::Synchronized));
        assert!(method.iter().any(|m| m == &Modifier::Native));
        assert!(!method.iter().any(|m| m == &Modifier::Interface));
    }

    #[test]
    fn forbidden_combos_are_caught() {
        // final + abstract — at every site.
        for site in [ModifierSite::Class, ModifierSite::Field, ModifierSite::Method] {
            let with_final = vec![Modifier::Final];
            assert!(
                forbidden_reason(&with_final, &Modifier::Abstract, site).is_some(),
                "{site:?}: final + abstract should be flagged"
            );
            let with_abs = vec![Modifier::Abstract];
            assert!(
                forbidden_reason(&with_abs, &Modifier::Final, site).is_some(),
                "{site:?}: abstract + final should be flagged"
            );
        }

        // Field-only: final + volatile.
        assert!(forbidden_reason(
            &[Modifier::Final],
            &Modifier::Volatile,
            ModifierSite::Field,
        )
        .is_some());

        // Method-only: abstract + static.
        assert!(forbidden_reason(
            &[Modifier::Abstract],
            &Modifier::Static,
            ModifierSite::Method,
        )
        .is_some());

        // No forbidden combo when independent flags are mixed.
        assert!(forbidden_reason(
            &[Modifier::Public, Modifier::Static],
            &Modifier::Final,
            ModifierSite::Field,
        )
        .is_none());
    }
}
