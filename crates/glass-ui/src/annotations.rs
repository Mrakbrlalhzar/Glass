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
    ///
    /// Legacy. New writes go to [`Self::by_op_index`]; old DB
    /// records still load as `MethodLine` until the per-bundle
    /// upgrade pass (`upgrade_method_line_to_op_index`) runs.
    by_method_line: Arc<HashMap<(String, u32), Annotation>>,
    /// Keyed by `(Class;->name+descriptor, op_index)` — the op
    /// index is the position within `SmaliMethod.ops`. Survives
    /// op insertions / deletions in the per-op editor.
    by_op_index: Arc<HashMap<(String, u32), Annotation>>,
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
            && self.by_op_index.is_empty()
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

    /// Op-index lookup: `(key, op_index)` where `key` is the
    /// `Class;->name+descriptor` form and `op_index` is the
    /// position within `SmaliMethod.ops`.
    pub fn at_op_index(&self, key: &str, op_index: u32) -> Option<&Annotation> {
        self.by_op_index.get(&(key.to_string(), op_index))
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
        let op_indices = self.by_op_index.iter().filter_map(|((k, op_index), v)| {
            let (class, name_sig) = k.split_once("->")?;
            Some((
                AnnotationKey::OpIndex {
                    class_jni: class.to_string(),
                    method_decl: name_sig.to_string(),
                    op_index: *op_index,
                },
                v,
            ))
        });
        addrs
            .chain(syms)
            .chain(classes)
            .chain(methods)
            .chain(method_lines)
            .chain(op_indices)
    }

    /// Total entry count across all key kinds.
    pub fn len(&self) -> usize {
        self.by_address.len()
            + self.by_symbol.len()
            + self.by_class.len()
            + self.by_method.len()
            + self.by_method_line.len()
            + self.by_op_index.len()
    }

    /// Snapshot every `MethodLine` entry as `(class_jni,
    /// method_decl, line_offset, annotation)`. Used by the
    /// bundle-load upgrade pass that walks each affected method,
    /// translates `line_offset` → `op_index`, and re-keys the
    /// in-memory + on-disk records.
    pub fn method_line_entries(&self) -> Vec<(String, String, u32, Annotation)> {
        self.by_method_line
            .iter()
            .filter_map(|((k, line), v)| {
                let (class, name_sig) = k.split_once("->")?;
                Some((class.to_string(), name_sig.to_string(), *line, v.clone()))
            })
            .collect()
    }

    /// Replace the index's `MethodLine` / `OpIndex` halves with
    /// fresh tables. Used by the upgrade pass after it's done
    /// the translation. The other key kinds aren't touched.
    pub fn replace_method_buckets(
        &mut self,
        new_method_line: HashMap<(String, u32), Annotation>,
        new_op_index: HashMap<(String, u32), Annotation>,
    ) {
        self.by_method_line = Arc::new(new_method_line);
        self.by_op_index = Arc::new(new_op_index);
    }
}

/// Map a `line_offset` (relative to the `.method` directive
/// itself, where 0 is the header) to the op index it lands on,
/// for one `SmaliMethod`. Returns `None` when the offset is
/// outside the method body or points at a non-op directive
/// line.
///
/// Walks the writer's output once and increments an op cursor
/// for every line that the writer emitted *for* an op. The
/// classification mirrors `smali_write::write_method` and the
/// shape of `SmaliOp` — labels, `.line` directives, catch
/// blocks, switches, arithmetic ops all consume one or more
/// lines per op.
pub fn line_offset_to_op_index(
    method: &smali::types::SmaliMethod,
    line_offset: u32,
) -> Option<u32> {
    // line_offset == 0 is the `.method` header — there's no op
    // there; caller should re-route to `AnnotationKey::Method`.
    if line_offset == 0 {
        return None;
    }
    let rendered = method.to_string();
    let mut op_cursor: u32 = 0;
    let mut skip_until: Option<&'static str> = None;
    for (i, raw) in rendered.lines().enumerate() {
        let t = raw.trim_start();
        // Multi-line op block in progress: every interior line
        // (including the closer) belongs to the same op, which
        // already had its cursor increment when the opener
        // appeared.
        if let Some(closer) = skip_until {
            if i as u32 == line_offset {
                // op_cursor was bumped when the opener was seen,
                // so the relevant op index is `op_cursor - 1`.
                return op_cursor.checked_sub(1);
            }
            if t.starts_with(closer) {
                skip_until = None;
            }
            continue;
        }
        // Prelude / structural lines aren't ops — skip them,
        // including when the user's annotation happened to live
        // on one of these. (They had no op identity in the
        // editor anyway.)
        let prelude = t.starts_with(".method ")
            || t.starts_with(".end method")
            || t.starts_with(".locals ")
            || t.starts_with(".registers ")
            || t.starts_with(".param")
            || t.starts_with(".annotation ")
            || t.starts_with(".end annotation")
            || t.starts_with(".subannotation ")
            || t.starts_with(".end subannotation")
            || t.is_empty();
        if prelude {
            if i as u32 == line_offset {
                return None;
            }
            continue;
        }
        // Multi-line op-block openers — bump cursor once, then
        // skip until the matching closer.
        let multi_close: Option<&'static str> = if t.starts_with(".array-data ") {
            Some(".end array-data")
        } else if t.starts_with(".packed-switch ") {
            Some(".end packed-switch")
        } else if t.starts_with(".sparse-switch") {
            Some(".end sparse-switch")
        } else {
            None
        };
        if let Some(closer) = multi_close {
            op_cursor = op_cursor.saturating_add(1);
            if i as u32 == line_offset {
                return op_cursor.checked_sub(1);
            }
            skip_until = Some(closer);
            continue;
        }
        // Single-line op row — increment, then either return
        // (if this is the target) or move on.
        op_cursor = op_cursor.saturating_add(1);
        if i as u32 == line_offset {
            return op_cursor.checked_sub(1);
        }
    }
    None
}

/// Walk every artifact's annotation index, translate any
/// remaining `MethodLine` entries into `OpIndex` ones via
/// `line_offset_to_op_index`, and persist the swap back to
/// the DB. Runs once at bundle-open time, while we have both
/// the lifted `SmaliClass` per artifact + the annotation
/// index in memory.
///
/// `smali_classes` is the loader's `HashMap<(ArtifactId,
/// class_jni), SmaliClass>` lookup.
///
/// Annotations whose `line_offset` we can't classify (e.g.
/// the method was edited away or the offset landed on a
/// prelude row) are left in place — they continue to render
/// via the legacy `MethodLine` lookup in the smali row
/// walker.
pub fn upgrade_method_line_to_op_index(
    db: &Database,
    indices: &mut HashMap<ArtifactId, AnnotationIndex>,
    smali_classes: &HashMap<(ArtifactId, String), smali::types::SmaliClass>,
) {
    for (aid, idx) in indices.iter_mut() {
        let entries = idx.method_line_entries();
        if entries.is_empty() {
            continue;
        }
        let mut keep_method_line: HashMap<(String, u32), Annotation> = HashMap::new();
        let mut new_op_index: HashMap<(String, u32), Annotation> = idx
            .by_op_index
            .as_ref()
            .clone();
        for (class_jni, method_decl, line_offset, annotation) in entries {
            let class = smali_classes.get(&(aid.clone(), class_jni.clone()));
            let method = class.and_then(|c| {
                c.methods.iter().find(|m| {
                    format!("{}{}", m.name, m.signature.to_jni()) == method_decl
                })
            });
            let op_index = method.and_then(|m| {
                line_offset_to_op_index(m, line_offset)
            });
            match op_index {
                Some(op_index) => {
                    // Persist the new OpIndex record.
                    let new_key = AnnotationKey::OpIndex {
                        class_jni: class_jni.clone(),
                        method_decl: method_decl.clone(),
                        op_index,
                    };
                    db.set_annotation(
                        aid.clone(),
                        new_key,
                        annotation.clone(),
                    );
                    // Drop the old MethodLine record.
                    let old_key = AnnotationKey::MethodLine(
                        class_jni.clone(),
                        method_decl.clone(),
                        line_offset,
                    );
                    db.clear_annotation(aid.clone(), old_key);
                    // Track the in-memory move.
                    new_op_index.insert(
                        (format!("{class_jni}->{method_decl}"), op_index),
                        annotation,
                    );
                }
                None => {
                    // Couldn't classify — keep the legacy entry
                    // in the in-memory index so the row walker
                    // still surfaces it. Not persisted again
                    // (the original record is unchanged).
                    keep_method_line.insert(
                        (format!("{class_jni}->{method_decl}"), line_offset),
                        annotation,
                    );
                }
            }
        }
        idx.replace_method_buckets(keep_method_line, new_op_index);
    }
    if let Err(e) = db.flush() {
        tracing::warn!(error = %e, "glass-ui: failed to flush DB after annotation upgrade");
    }
}

/// Inverse of [`line_offset_to_op_index`]: given an op index,
/// returns the line offset at which that op renders within the
/// method body. Used by `navigate_to_annotation` to scroll a
/// smali tab to the right line when the user clicks an OpIndex
/// entry in the annotations pane.
pub fn op_index_to_line_offset(
    method: &smali::types::SmaliMethod,
    op_index: u32,
) -> Option<u32> {
    let rendered = method.to_string();
    let mut op_cursor: u32 = 0;
    let mut skip_until: Option<&'static str> = None;
    for (i, raw) in rendered.lines().enumerate() {
        let t = raw.trim_start();
        if let Some(closer) = skip_until {
            if t.starts_with(closer) {
                skip_until = None;
            }
            continue;
        }
        let prelude = t.starts_with(".method ")
            || t.starts_with(".end method")
            || t.starts_with(".locals ")
            || t.starts_with(".registers ")
            || t.starts_with(".param")
            || t.starts_with(".annotation ")
            || t.starts_with(".end annotation")
            || t.starts_with(".subannotation ")
            || t.starts_with(".end subannotation")
            || t.is_empty();
        if prelude {
            continue;
        }
        let multi_close: Option<&'static str> = if t.starts_with(".array-data ") {
            Some(".end array-data")
        } else if t.starts_with(".packed-switch ") {
            Some(".end packed-switch")
        } else if t.starts_with(".sparse-switch") {
            Some(".end sparse-switch")
        } else {
            None
        };
        if op_cursor == op_index {
            return Some(i as u32);
        }
        op_cursor = op_cursor.saturating_add(1);
        if let Some(closer) = multi_close {
            skip_until = Some(closer);
        }
    }
    None
}

#[cfg(test)]
mod line_to_op_tests {
    use super::*;
    use smali::types::SmaliClass;

    fn parse_one_method(body: &str) -> smali::types::SmaliMethod {
        let wrapped = format!(
            ".class public Lglass/internal/T;\n\
             .super Ljava/lang/Object;\n\
             {body}\n"
        );
        let c = SmaliClass::from_smali(&wrapped).expect("parse");
        c.methods.into_iter().next().expect("at least one method")
    }

    #[test]
    fn maps_op_lines_in_order() {
        let m = parse_one_method(
            ".method public foo()V\n\
             .locals 0\n\
             nop\n\
             nop\n\
             return-void\n\
             .end method",
        );
        // Rendered method emits .method, .locals, then three op
        // lines, then .end method. The three nops/return-void
        // sit at line offsets 2, 3, 4 → op indices 0, 1, 2.
        assert_eq!(line_offset_to_op_index(&m, 0), None); // header
        assert_eq!(line_offset_to_op_index(&m, 1), None); // .locals
        assert_eq!(line_offset_to_op_index(&m, 2), Some(0));
        assert_eq!(line_offset_to_op_index(&m, 3), Some(1));
        assert_eq!(line_offset_to_op_index(&m, 4), Some(2));
    }

    #[test]
    fn returns_none_for_out_of_range_offsets() {
        let m = parse_one_method(
            ".method public foo()V\n\
             .locals 0\n\
             return-void\n\
             .end method",
        );
        assert_eq!(line_offset_to_op_index(&m, 999), None);
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
        let mut by_op_index = HashMap::new();
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
                AnnotationKey::OpIndex {
                    class_jni,
                    method_decl,
                    op_index,
                } => {
                    by_op_index.insert(
                        (format!("{class_jni}->{method_decl}"), op_index),
                        v,
                    );
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
                by_op_index: Arc::new(by_op_index),
            },
        );
    }
    out
}
