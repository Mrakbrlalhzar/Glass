//! In-memory annotation index used by the GUI.
//!
//! Loaded once at bundle-open time from `glass_db::Database`, then
//! consulted by the listing + smali renderers on every row. Writes
//! (Phase 4) will mutate in place via `Arc::make_mut`.
//!
//! Precedence rule for listing rows: address-keyed annotations beat
//! symbol-keyed ones at the same row. Symbol-keyed acts as a
//! fallback when no address key is set. The renderer should call
//! [`AnnotationIndex::for_row`] which encodes this.

use std::collections::HashMap;
use std::sync::Arc;

use glass_db::{Annotation, AnnotationKey, ArtifactId, Database};

/// Per-artifact lookup table. Cheap to clone — every field is
/// an Arc behind a HashMap.
#[derive(Clone, Default, Debug)]
pub struct AnnotationIndex {
    by_address: Arc<HashMap<u64, Annotation>>,
    by_symbol: Arc<HashMap<String, Annotation>>,
    by_class: Arc<HashMap<String, Annotation>>,
    /// Keyed by `Class;->name+descriptor` (the same key form the
    /// CLI uses in `set-rename --key-kind method`).
    by_method: Arc<HashMap<String, Annotation>>,
    /// Keyed by `(Class;->name+descriptor, line_offset)` — the
    /// line offset is 0-indexed from the `.method` line itself.
    by_method_line: Arc<HashMap<(String, u32), Annotation>>,
}

impl AnnotationIndex {
    /// Returns a value when at least one row has any annotation.
    /// Lets the renderer skip the lookup loop entirely on the
    /// (overwhelmingly common) all-empty case.
    pub fn is_empty(&self) -> bool {
        self.by_address.is_empty()
            && self.by_symbol.is_empty()
            && self.by_class.is_empty()
            && self.by_method.is_empty()
            && self.by_method_line.is_empty()
    }

    /// Address-keyed annotation, if any.
    pub fn at_address(&self, addr: u64) -> Option<&Annotation> {
        self.by_address.get(&addr)
    }

    /// Symbol-keyed annotation, if any. `name` is matched verbatim
    /// against `Symbol(name)` keys — usually the display name.
    pub fn at_symbol(&self, name: &str) -> Option<&Annotation> {
        self.by_symbol.get(name)
    }

    pub fn at_class(&self, class_jni: &str) -> Option<&Annotation> {
        self.by_class.get(class_jni)
    }

    pub fn at_method(&self, key: &str) -> Option<&Annotation> {
        self.by_method.get(key)
    }

    /// Method-line lookup: `(key, line_offset)` where `key` is the
    /// `Class;->name+descriptor` form and `line_offset == 0`
    /// targets the `.method` directive itself.
    pub fn at_method_line(&self, key: &str, line_offset: u32) -> Option<&Annotation> {
        self.by_method_line.get(&(key.to_string(), line_offset))
    }

    /// Resolve a listing row by precedence: address beats symbol.
    /// Returns the winning annotation, or `None` if both are unset.
    pub fn for_row(&self, addr: u64, symbol_name: Option<&str>) -> Option<&Annotation> {
        if let Some(a) = self.at_address(addr) {
            return Some(a);
        }
        symbol_name.and_then(|n| self.at_symbol(n))
    }

    /// Iterate every annotation in the index. Used by the
    /// annotations pane.
    pub fn iter(&self) -> impl Iterator<Item = (AnnotationKey, &Annotation)> + '_ {
        let addrs = self
            .by_address
            .iter()
            .map(|(a, v)| (AnnotationKey::Address(*a), v));
        let syms = self
            .by_symbol
            .iter()
            .map(|(s, v)| (AnnotationKey::Symbol(s.clone()), v));
        let classes = self
            .by_class
            .iter()
            .map(|(c, v)| (AnnotationKey::Class(c.clone()), v));
        let methods = self.by_method.iter().filter_map(|(k, v)| {
            // Stored as "Class;->name+descriptor". Split on "->".
            let (class, name_sig) = k.split_once("->")?;
            Some((
                AnnotationKey::Method(class.to_string(), name_sig.to_string()),
                v,
            ))
        });
        let method_lines = self.by_method_line.iter().filter_map(|((k, line), v)| {
            let (class, name_sig) = k.split_once("->")?;
            Some((
                AnnotationKey::MethodLine(
                    class.to_string(),
                    name_sig.to_string(),
                    *line,
                ),
                v,
            ))
        });
        addrs.chain(syms).chain(classes).chain(methods).chain(method_lines)
    }

    /// Total entry count across all key kinds.
    pub fn len(&self) -> usize {
        self.by_address.len()
            + self.by_symbol.len()
            + self.by_class.len()
            + self.by_method.len()
            + self.by_method_line.len()
    }
}

/// Per-artifact index built from the DB. Returns an empty index
/// when no annotations exist or when the DB read fails (loader
/// shouldn't refuse to open a bundle just because annotation
/// load hiccuped — log + carry on).
pub fn load_for_artifacts(
    db: &Database,
    artifact_ids: &[ArtifactId],
) -> HashMap<ArtifactId, AnnotationIndex> {
    let mut out = HashMap::new();
    for aid in artifact_ids {
        let entries = match db.load_annotations(aid) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    artifact = %aid,
                    error = %e,
                    "glass-ui: failed to load annotations; treating as empty"
                );
                continue;
            }
        };
        if entries.is_empty() {
            continue;
        }
        let mut by_address = HashMap::new();
        let mut by_symbol = HashMap::new();
        let mut by_class = HashMap::new();
        let mut by_method = HashMap::new();
        let mut by_method_line = HashMap::new();
        for (k, v) in entries {
            match k {
                AnnotationKey::Address(a) => {
                    by_address.insert(a, v);
                }
                AnnotationKey::Symbol(s) => {
                    by_symbol.insert(s, v);
                }
                AnnotationKey::Class(c) => {
                    by_class.insert(c, v);
                }
                AnnotationKey::Method(c, m) => {
                    by_method.insert(format!("{c}->{m}"), v);
                }
                AnnotationKey::MethodLine(c, m, line) => {
                    by_method_line.insert((format!("{c}->{m}"), line), v);
                }
            }
        }
        out.insert(
            aid.clone(),
            AnnotationIndex {
                by_address: Arc::new(by_address),
                by_symbol: Arc::new(by_symbol),
                by_class: Arc::new(by_class),
                by_method: Arc::new(by_method),
                by_method_line: Arc::new(by_method_line),
            },
        );
    }
    out
}
