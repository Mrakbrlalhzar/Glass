//! In-memory typed-smali edit registry.
//!
//! Sits alongside `edits::EditRegistry` (which handles byte-level
//! splices for AArch64 instructions, hex bytes, and C strings).
//! Where that one stages `(artifact, vaddr) → new_bytes`, this one
//! stages `(artifact, class_jni) → SmaliClass` — a full typed
//! replacement of one DEX class.
//!
//! Why typed rather than text:
//!   - The smali editor uses the parsed model directly (one row per
//!     `SmaliOp`, structured popovers for modifiers/signatures, etc).
//!     Storing edits as a typed value side-steps re-parsing on every
//!     keystroke and naturally rejects malformed states at the cell
//!     boundary.
//!   - DEX re-emission (M2.2) wants a `SmaliClass`, not a string.
//!   - Annotations key on `(class, method, op_index)` — also typed.
//!
//! In-memory only for v1. Closing the bundle drops every staged
//! edit; M2.2 will route them through the existing `export-patched`
//! path so they land in a re-zipped APK on demand.

use std::collections::HashMap;

use smali::types::SmaliClass;

/// Identifies a DEX class within a bundle. `class_jni` is the JNI
/// signature (e.g. `Lcom/example/Foo;`) which is stable across DEX
/// reshuffles — the same key the persistence layer uses for tabs
/// and annotations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SmaliEditKey {
    pub artifact: glass_db::ArtifactId,
    pub class_jni: String,
}

/// One staged class edit.
#[derive(Debug, Clone)]
pub struct SmaliEdit {
    pub key: SmaliEditKey,
    /// Edited class. The renderer reads this and the export path
    /// (M2.2) splices it back into the DEX.
    pub modified: SmaliClass,
}

#[derive(Default, Debug, Clone)]
pub struct SmaliEditRegistry {
    by_key: HashMap<SmaliEditKey, SmaliEdit>,
}

impl SmaliEditRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Stage (or replace) an edit.
    pub fn insert(&mut self, edit: SmaliEdit) {
        self.by_key.insert(edit.key.clone(), edit);
    }

    /// Look up an edit for a specific class. Returns `None` if the
    /// class is untouched.
    pub fn get(&self, artifact: &glass_db::ArtifactId, class_jni: &str) -> Option<&SmaliEdit> {
        // Avoid building an owned key for the lookup by going via
        // a small adapter — the HashMap's Equivalent lookup would
        // need extra trait wiring, and edits are rare enough that
        // the clone cost is irrelevant.
        let probe = SmaliEditKey {
            artifact: artifact.clone(),
            class_jni: class_jni.to_string(),
        };
        self.by_key.get(&probe)
    }

    /// Drop an edit (revert).
    pub fn remove(&mut self, artifact: &glass_db::ArtifactId, class_jni: &str) -> Option<SmaliEdit> {
        let probe = SmaliEditKey {
            artifact: artifact.clone(),
            class_jni: class_jni.to_string(),
        };
        self.by_key.remove(&probe)
    }

    pub fn clear(&mut self) {
        self.by_key.clear();
    }

    /// Does the staged class differ from `original` *in its
    /// class-declaration portion only* — i.e. modifiers, super
    /// class, implements list, source-file hint, or class-level
    /// annotations? Field / method edits don't count. Used by the
    /// smali renderer to tint `.class` / `.super` / `.implements`
    /// / `.source` lines only when those rows are actually
    /// affected by the staged edit.
    pub fn class_decl_differs(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        original: &SmaliClass,
    ) -> bool {
        let Some(edit) = self.get(artifact, class_jni) else {
            return false;
        };
        class_decl_differs(original, &edit.modified)
    }

    /// Names + signatures of fields whose staged version differs
    /// from the original. Both sides are matched by their
    /// position in the original class — if a field was added or
    /// removed wholesale, callers should compare lengths and fall
    /// back to a coarser "class differs" check. Today the form
    /// popovers only replace existing fields in place, so this
    /// vector-by-vector compare is enough.
    pub fn edited_fields(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        original: &SmaliClass,
    ) -> Vec<(String, String)> {
        let Some(edit) = self.get(artifact, class_jni) else {
            return Vec::new();
        };
        diff_members(&original.fields, &edit.modified.fields, |f| {
            (f.name.clone(), f.signature.to_jni())
        }, field_text)
    }

    /// Names + JNI signatures of methods that differ between
    /// `original` and the staged version. Same semantics as
    /// `edited_fields` — position-keyed.
    pub fn edited_methods(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        original: &SmaliClass,
    ) -> Vec<(String, String)> {
        let Some(edit) = self.get(artifact, class_jni) else {
            return Vec::new();
        };
        diff_members(&original.methods, &edit.modified.methods, |m| {
            (m.name.clone(), m.signature.to_jni())
        }, method_text)
    }
}

/// Generic member-diff: returns `(name, sig)` identifiers for the
/// items at positions where `text(a) != text(b)`. Length mismatch
/// returns every item that has no positional peer too — those are
/// strictly "different" by virtue of being absent on one side.
fn diff_members<T, K, R>(
    original: &[T],
    modified: &[T],
    key: K,
    text: R,
) -> Vec<(String, String)>
where
    K: Fn(&T) -> (String, String),
    R: Fn(&T) -> String,
{
    let mut out = Vec::new();
    let n = original.len().max(modified.len());
    for i in 0..n {
        match (original.get(i), modified.get(i)) {
            (Some(a), Some(b)) => {
                if text(a) != text(b) {
                    out.push(key(b));
                }
            }
            (Some(a), None) => out.push(key(a)),
            (None, Some(b)) => out.push(key(b)),
            (None, None) => {}
        }
    }
    out
}

fn field_text(f: &smali::types::SmaliField) -> String {
    f.to_string()
}

fn method_text(m: &smali::types::SmaliMethod) -> String {
    m.to_string()
}

/// `true` when the parts of a class that render on `.class /
/// .super / .implements / .source` lines or as class-level
/// annotations differ between `original` and `modified`. Fields
/// and methods are intentionally ignored.
///
/// Both sides are reduced to a "decl-only" clone (fields and
/// methods blanked) and serialised via `to_smali()`. Comparing
/// the rendered text rather than walking the in-memory structs
/// catches any divergence in how the parser canonicalises edge
/// cases — the writer is the authoritative source of truth for
/// what ends up in the exported DEX anyway.
fn class_decl_differs(original: &SmaliClass, modified: &SmaliClass) -> bool {
    decl_only_text(original) != decl_only_text(modified)
}

fn decl_only_text(c: &SmaliClass) -> String {
    let mut shell = c.clone();
    shell.fields.clear();
    shell.methods.clear();
    shell.to_smali()
}


impl SmaliEditRegistry {
    /// Iterate staged edits in a stable order. We sort at call time
    /// — there are at most a handful of edits per session.
    pub fn entries(&self) -> Vec<&SmaliEdit> {
        let mut out: Vec<&SmaliEdit> = self.by_key.values().collect();
        out.sort_by(|a, b| {
            let aa = (a.key.artifact.to_string(), &a.key.class_jni);
            let bb = (b.key.artifact.to_string(), &b.key.class_jni);
            aa.cmp(&bb)
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smali::types::ObjectIdentifier;

    fn fake_class(jni: &str) -> SmaliClass {
        SmaliClass {
            name: ObjectIdentifier::from_jni_type(jni),
            modifiers: vec![],
            source: None,
            super_class: ObjectIdentifier::from_jni_type("Ljava/lang/Object;"),
            implements: vec![],
            annotations: vec![],
            fields: vec![],
            methods: vec![],
            file_path: None,
        }
    }

    #[test]
    fn insert_get_remove_round_trip() {
        let mut reg = SmaliEditRegistry::new();
        let aid = glass_db::ArtifactId::from_bytes(b"dex-1");
        let jni = "Lcom/example/Foo;";
        assert!(reg.is_empty());
        assert!(reg.get(&aid, jni).is_none());

        reg.insert(SmaliEdit {
            key: SmaliEditKey { artifact: aid.clone(), class_jni: jni.to_string() },
            modified: fake_class(jni),
        });
        assert_eq!(reg.len(), 1);
        assert!(reg.get(&aid, jni).is_some());

        // Different jni on the same artifact is a separate entry.
        assert!(reg.get(&aid, "Lcom/example/Bar;").is_none());

        let removed = reg.remove(&aid, jni);
        assert!(removed.is_some());
        assert!(reg.is_empty());
    }

    #[test]
    fn insert_replaces_existing_entry() {
        let mut reg = SmaliEditRegistry::new();
        let aid = glass_db::ArtifactId::from_bytes(b"dex-1");
        let jni = "Lcom/example/Foo;";
        let key = SmaliEditKey { artifact: aid.clone(), class_jni: jni.to_string() };
        let mut c1 = fake_class(jni);
        c1.source = Some("Foo.java".into());
        let mut c2 = fake_class(jni);
        c2.source = Some("FooEdited.java".into());
        reg.insert(SmaliEdit { key: key.clone(), modified: c1 });
        reg.insert(SmaliEdit { key, modified: c2 });
        assert_eq!(reg.len(), 1);
        assert_eq!(
            reg.get(&aid, jni).unwrap().modified.source.as_deref(),
            Some("FooEdited.java"),
        );
    }

    #[test]
    fn class_decl_differs_ignores_field_only_changes() {
        use smali::types::{Modifier, SmaliField, TypeSignature};
        let mut reg = SmaliEditRegistry::new();
        let aid = glass_db::ArtifactId::from_bytes(b"dex-1");
        let jni = "Lcom/example/Foo;";
        let original = fake_class(jni);
        let mut modified = original.clone();
        // Add a field — class declaration is untouched.
        modified.fields.push(SmaliField {
            name: "x".into(),
            modifiers: vec![Modifier::Private],
            signature: TypeSignature::from_jni("I"),
            initial_value: None,
            annotations: vec![],
        });
        reg.insert(SmaliEdit {
            key: SmaliEditKey {
                artifact: aid.clone(),
                class_jni: jni.into(),
            },
            modified,
        });
        assert!(!reg.class_decl_differs(&aid, jni, &original));
    }

    #[test]
    fn round_trip_preserves_class_decl() {
        // The external editor pipeline does class.to_smali() →
        // edit text → SmaliClass::from_smali(text). If that
        // round-trip isn't byte-identical for the class-decl
        // parts, `class_decl_differs` will fire spuriously on a
        // field-only external edit. Guard against that.
        use smali::types::{Modifier, ObjectIdentifier, SmaliField, TypeSignature};
        let original = SmaliClass {
            name: ObjectIdentifier::from_jni_type("Lcom/example/Foo;"),
            modifiers: vec![Modifier::Public, Modifier::Final],
            source: Some("Foo.java".into()),
            super_class: ObjectIdentifier::from_jni_type("Ljava/lang/Object;"),
            implements: vec![
                ObjectIdentifier::from_jni_type("Ljava/io/Serializable;"),
                ObjectIdentifier::from_jni_type("Ljava/lang/Cloneable;"),
            ],
            annotations: vec![],
            fields: vec![SmaliField {
                name: "count".into(),
                modifiers: vec![Modifier::Private],
                signature: TypeSignature::from_jni("I"),
                initial_value: None,
                annotations: vec![],
            }],
            methods: vec![],
            file_path: None,
        };
        // Round-trip via smali text.
        let text = original.to_smali();
        let reparsed = SmaliClass::from_smali(&text)
            .expect("round-trip from_smali should succeed");
        let aid = glass_db::ArtifactId::from_bytes(b"dex-1");
        let mut reg = SmaliEditRegistry::new();
        reg.insert(SmaliEdit {
            key: SmaliEditKey {
                artifact: aid.clone(),
                class_jni: "Lcom/example/Foo;".into(),
            },
            modified: reparsed,
        });
        assert!(
            !reg.class_decl_differs(&aid, "Lcom/example/Foo;", &original),
            "class-decl round-trip should be identity-preserving — \
             a field-only external edit would otherwise tint the \
             header"
        );
    }

    #[test]
    fn class_decl_differs_detects_super_change() {
        let mut reg = SmaliEditRegistry::new();
        let aid = glass_db::ArtifactId::from_bytes(b"dex-1");
        let jni = "Lcom/example/Foo;";
        let original = fake_class(jni);
        let mut modified = original.clone();
        modified.super_class =
            smali::types::ObjectIdentifier::from_jni_type("Ljava/util/HashMap;");
        reg.insert(SmaliEdit {
            key: SmaliEditKey {
                artifact: aid.clone(),
                class_jni: jni.into(),
            },
            modified,
        });
        assert!(reg.class_decl_differs(&aid, jni, &original));
    }

    #[test]
    fn entries_sorted_by_artifact_then_class() {
        let mut reg = SmaliEditRegistry::new();
        let aid_a = glass_db::ArtifactId::from_bytes(b"aaaa");
        let aid_b = glass_db::ArtifactId::from_bytes(b"bbbb");
        // Insert in shuffled order; entries() should sort it.
        for (aid, jni) in [
            (&aid_b, "Lcom/example/Zed;"),
            (&aid_a, "Lcom/example/Bar;"),
            (&aid_a, "Lcom/example/Foo;"),
        ] {
            reg.insert(SmaliEdit {
                key: SmaliEditKey { artifact: aid.clone(), class_jni: jni.into() },
                modified: fake_class(jni),
            });
        }
        let names: Vec<_> = reg.entries().iter().map(|e| e.key.class_jni.clone()).collect();
        assert_eq!(names, vec![
            "Lcom/example/Bar;".to_string(),
            "Lcom/example/Foo;".to_string(),
            "Lcom/example/Zed;".to_string(),
        ]);
    }
}
