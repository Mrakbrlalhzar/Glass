//! SVG icons used in the navigator tree.
//!
//! Icons are baked into the binary via `include_bytes!` and
//! resolved through gpui's [`AssetSource`] trait — `svg().path()`
//! looks up each name in [`IconAssets`] at render time. Keeping
//! them as separate files keeps the SVGs editable / linted / diff-
//! friendly; baking sidesteps any runtime filesystem layout.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

const JAVA: &[u8] = include_bytes!("../assets/icons/java.svg");
const KOTLIN: &[u8] = include_bytes!("../assets/icons/kotlin.svg");
const SMALI: &[u8] = include_bytes!("../assets/icons/smali.svg");
const HEX: &[u8] = include_bytes!("../assets/icons/hex.svg");
const MANIFEST: &[u8] = include_bytes!("../assets/icons/manifest.svg");
const LISTING: &[u8] = include_bytes!("../assets/icons/listing.svg");
const SECTION_MAP: &[u8] = include_bytes!("../assets/icons/section-map.svg");

/// Asset source registered with gpui at app startup. Resolves
/// the `icons/<name>.svg` paths used by `svg().path(...)` to
/// the matching `include_bytes!` blob.
pub struct IconAssets;

impl AssetSource for IconAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/java.svg" => Some(JAVA),
            "icons/kotlin.svg" => Some(KOTLIN),
            "icons/smali.svg" => Some(SMALI),
            "icons/hex.svg" => Some(HEX),
            "icons/manifest.svg" => Some(MANIFEST),
            "icons/listing.svg" => Some(LISTING),
            "icons/section-map.svg" => Some(SECTION_MAP),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        // gpui doesn't list-iterate icons at runtime — only loads
        // by exact path. An empty list is fine.
        Ok(Vec::new())
    }
}

/// Pick the right icon path for a `LeafKind`. For `SmaliClass`
/// the source extension (from `SmaliClass.source` — typically
/// `Foo.java` / `Foo.kt`) selects between the Java and Kotlin
/// variants; missing or unrecognised → generic smali icon.
pub fn icon_path_for_leaf(
    kind: &crate::LeafKind,
    source_hint: Option<&str>,
) -> &'static str {
    use crate::LeafKind as L;
    match kind {
        L::SmaliClass { .. } => source_kind_icon(source_hint),
        L::Listing { .. } => "icons/listing.svg",
        L::Hex { .. } => "icons/hex.svg",
        L::SectionMap { .. } => "icons/section-map.svg",
        L::Manifest => "icons/manifest.svg",
        // CFG / DexCallGraph / other tab-driven views aren't
        // navigator leaves today; fall back to the listing icon
        // so we never panic.
        _ => "icons/listing.svg",
    }
}

/// Derive the per-leaf icon path table at bundle-load time.
/// `class_source` looks up `SmaliClass.source` for a JNI sig —
/// the loader passes a closure that consults `smali_classes`.
pub fn leaf_icons_for(
    kinds: &[crate::LeafKind],
    mut class_source: impl FnMut(&str) -> Option<String>,
) -> Vec<&'static str> {
    kinds
        .iter()
        .map(|k| {
            let source = match k {
                crate::LeafKind::SmaliClass { class_jni } => class_source(class_jni),
                _ => None,
            };
            icon_path_for_leaf(k, source.as_deref())
        })
        .collect()
}

fn source_kind_icon(source: Option<&str>) -> &'static str {
    let Some(s) = source else { return "icons/smali.svg" };
    let lower = s.to_ascii_lowercase();
    if lower.ends_with(".kt") {
        "icons/kotlin.svg"
    } else if lower.ends_with(".java") {
        "icons/java.svg"
    } else {
        "icons/smali.svg"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_picks_language_icon() {
        assert_eq!(source_kind_icon(Some("Foo.java")), "icons/java.svg");
        assert_eq!(source_kind_icon(Some("Foo.kt")), "icons/kotlin.svg");
        assert_eq!(source_kind_icon(Some("Foo.unknown")), "icons/smali.svg");
        assert_eq!(source_kind_icon(None), "icons/smali.svg");
    }

    #[test]
    fn all_baked_assets_resolve() {
        let s = IconAssets;
        for path in [
            "icons/java.svg",
            "icons/kotlin.svg",
            "icons/smali.svg",
            "icons/hex.svg",
            "icons/manifest.svg",
            "icons/listing.svg",
            "icons/section-map.svg",
        ] {
            assert!(s.load(path).unwrap().is_some(), "missing {path}");
        }
        assert!(s.load("icons/nope.svg").unwrap().is_none());
    }
}
