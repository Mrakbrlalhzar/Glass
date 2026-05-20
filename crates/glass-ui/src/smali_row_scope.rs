//! Walk a smali leaf's lines once and tag each row with the
//! structural element it belongs to. Used by the renderer to
//! decide tinting and by the click handlers to route to the
//! right popover.
//!
//! Scopes match the structural editor's units:
//!
//!   * `ClassDecl` — `.class` / `.super` / `.implements` /
//!     `.source` lines and any class-level `.annotation` block.
//!   * `Field { name, signature }` — a `.field` line plus any
//!     nested `.annotation … .end annotation` block that
//!     belongs to it.
//!   * `Method { name, signature }` — a `.method` line and
//!     everything down through the matching `.end method`.
//!   * `Untouched` — blank lines, lines outside any of the
//!     above (which today shouldn't happen, but we don't want
//!     to crash if smali grows new top-level constructs).

use gpui::SharedString;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowScope {
    Untouched,
    ClassDecl,
    Field {
        name: String,
        signature: String,
    },
    Method {
        name: String,
        signature: String,
    },
}

/// Build the per-row scope mask for `lines`. Length equals
/// `lines.len()`.
pub fn compute(lines: &[SharedString]) -> Vec<RowScope> {
    let mut out = vec![RowScope::Untouched; lines.len()];
    let mut state = State::ClassDecl { in_annotation: false };
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim_start();
        state = step(state, t, &mut out, i);
    }
    out
}

#[derive(Debug, Clone)]
enum State {
    ClassDecl {
        in_annotation: bool,
    },
    Field {
        name: String,
        signature: String,
        in_annotation: bool,
    },
    Method {
        name: String,
        signature: String,
    },
}

fn step(state: State, t: &str, out: &mut [RowScope], i: usize) -> State {
    match state {
        State::ClassDecl { in_annotation } => {
            // Field header — switch to a Field scope. The line
            // itself counts as the field, not the class.
            if let Some((name, sig)) = parse_field_decl(t) {
                out[i] = RowScope::Field { name: name.clone(), signature: sig.clone() };
                return State::Field { name, signature: sig, in_annotation: false };
            }
            // Method header — switch to a Method scope. The line
            // itself counts as the method.
            if let Some((name, sig)) = parse_method_decl(t) {
                out[i] = RowScope::Method { name: name.clone(), signature: sig.clone() };
                return State::Method { name, signature: sig };
            }
            // Still in class scope — annotation block or
            // declaration line, both tag as ClassDecl.
            if in_annotation {
                out[i] = RowScope::ClassDecl;
                if t.starts_with(".end annotation") {
                    return State::ClassDecl { in_annotation: false };
                }
                return State::ClassDecl { in_annotation: true };
            }
            if t.starts_with(".annotation ") {
                out[i] = RowScope::ClassDecl;
                return State::ClassDecl { in_annotation: true };
            }
            if line_is_class_decl(t) {
                out[i] = RowScope::ClassDecl;
            }
            State::ClassDecl { in_annotation: false }
        }
        State::Field {
            name,
            signature,
            in_annotation,
        } => {
            // A new field or method header ends the current
            // field scope.
            if let Some((next_name, next_sig)) = parse_field_decl(t) {
                out[i] = RowScope::Field {
                    name: next_name.clone(),
                    signature: next_sig.clone(),
                };
                return State::Field {
                    name: next_name,
                    signature: next_sig,
                    in_annotation: false,
                };
            }
            if let Some((next_name, next_sig)) = parse_method_decl(t) {
                out[i] = RowScope::Method {
                    name: next_name.clone(),
                    signature: next_sig.clone(),
                };
                return State::Method { name: next_name, signature: next_sig };
            }
            // Otherwise: any inner row (`.annotation` block,
            // `.end field`, etc.) is still part of this field.
            out[i] = RowScope::Field {
                name: name.clone(),
                signature: signature.clone(),
            };
            if t.starts_with(".end field") {
                return State::ClassDecl { in_annotation: false };
            }
            if in_annotation {
                if t.starts_with(".end annotation") {
                    return State::Field {
                        name,
                        signature,
                        in_annotation: false,
                    };
                }
                return State::Field { name, signature, in_annotation: true };
            }
            if t.starts_with(".annotation ") {
                return State::Field { name, signature, in_annotation: true };
            }
            // Inline `.field` form has no body — but blank lines
            // or unrelated content following stop the scope.
            // Treat blank lines as ending the field (matches how
            // the writer emits a separator).
            if t.is_empty() {
                out[i] = RowScope::Untouched;
                return State::ClassDecl { in_annotation: false };
            }
            State::Field { name, signature, in_annotation }
        }
        State::Method { name, signature } => {
            out[i] = RowScope::Method {
                name: name.clone(),
                signature: signature.clone(),
            };
            if t.starts_with(".end method") {
                return State::ClassDecl { in_annotation: false };
            }
            State::Method { name, signature }
        }
    }
}

fn line_is_class_decl(t: &str) -> bool {
    t.starts_with(".class ")
        || t.starts_with(".super ")
        || t.starts_with(".implements ")
        || t.starts_with(".source ")
}

/// `.field <mods> name:Sig[ = …]` → `(name, sig)`. None if the
/// line isn't a `.field` header.
fn parse_field_decl(t: &str) -> Option<(String, String)> {
    let rest = t.strip_prefix(".field ")?.trim_start();
    let head = match rest.find(" = ") {
        Some(eq) => &rest[..eq],
        None => rest,
    };
    let token = head.split_whitespace().last()?;
    let (name, sig) = token.split_once(':')?;
    if name.is_empty() || sig.is_empty() {
        return None;
    }
    Some((name.to_string(), sig.to_string()))
}

/// `.method <mods> [constructor ]name(<sig>)ret` → `(name, "(<sig>)ret")`.
fn parse_method_decl(t: &str) -> Option<(String, String)> {
    let rest = t.strip_prefix(".method ")?.trim_start();
    let paren = rest.find('(')?;
    let head = &rest[..paren];
    let sig_part = &rest[paren..];
    let name = head.split_whitespace().last()?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), sig_part.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_strs(ss: &[&str]) -> Vec<SharedString> {
        ss.iter().map(|s| SharedString::from(*s)).collect()
    }

    #[test]
    fn classifies_each_scope() {
        let lines = from_strs(&[
            ".class public Lcom/Foo;",
            ".super Ljava/lang/Object;",
            ".annotation runtime Ldagger/Module;",
            ".end annotation",
            "",
            ".field private count:I",
            "",
            ".method public foo()V",
            "    return-void",
            ".end method",
            ".method public static bar(I)Z",
            "    const/4 v0, 0x1",
            "    return v0",
            ".end method",
        ]);
        let scopes = compute(&lines);
        assert_eq!(scopes[0], RowScope::ClassDecl);
        assert_eq!(scopes[1], RowScope::ClassDecl);
        assert_eq!(scopes[2], RowScope::ClassDecl);
        assert_eq!(scopes[3], RowScope::ClassDecl);
        assert_eq!(scopes[4], RowScope::Untouched);
        assert_eq!(
            scopes[5],
            RowScope::Field { name: "count".into(), signature: "I".into() }
        );
        assert!(matches!(scopes[7], RowScope::Method { ref name, .. } if name == "foo"));
        assert!(matches!(scopes[8], RowScope::Method { ref name, .. } if name == "foo"));
        assert!(matches!(scopes[9], RowScope::Method { ref name, .. } if name == "foo"));
        assert!(matches!(scopes[10], RowScope::Method { ref name, .. } if name == "bar"));
        assert!(matches!(scopes[13], RowScope::Method { ref name, .. } if name == "bar"));
    }

    #[test]
    fn field_with_annotation_block() {
        let lines = from_strs(&[
            ".class public Lcom/Foo;",
            ".super Ljava/lang/Object;",
            ".field private count:I",
            "    .annotation runtime Ldagger/Provides;",
            "    .end annotation",
            ".end field",
            "",
            ".method public foo()V",
            ".end method",
        ]);
        let scopes = compute(&lines);
        for i in 2..=5 {
            assert!(
                matches!(scopes[i], RowScope::Field { ref name, .. } if name == "count"),
                "row {i} should be Field(count): {:?}",
                scopes[i]
            );
        }
        assert!(matches!(scopes[7], RowScope::Method { ref name, .. } if name == "foo"));
    }
}
