//! Rendered text + typed-chunk row model for a Swift nominal type.
//!
//! Parallels [`crate::objc_format`] for the Objective-C side. Walks a
//! [`SwiftType`] (from `armv8_encode::container`) and produces a
//! sequence of rows the glass-ui Swift tab renders. Rows reuse the
//! same `Chunk` / `ChunkKind` model as the listing, so vtable
//! [`ChunkKind::Address`] chunks with `target: Some(addr)` become
//! clickable jump-to-listing links via the existing UI wiring.
//!
//! Like `objc_format`, the rendering is biased toward readability
//! rather than faithful Swift syntax — the intent is "show the user
//! the fields and vtable entries at a glance, with addresses they can
//! jump into".

use armv8_encode::container::{SwiftType, SwiftTypeKind};

use crate::format::{Chunk, ChunkKind};
use crate::symbol_map::demangle;

/// Best-effort demangle for a Swift mangled type name. Plain ASCII
/// type names (rare but possible for `@objc` Swift classes) pass
/// through untouched; modern Swift ABI mangling (`$s...` /
/// `_$s...`) is handled by `symbolic-demangle`.
pub fn pretty_swift_type_name(raw: &str) -> String {
    if raw.is_empty() {
        return raw.to_string();
    }
    // `symbolic-demangle` expects a leading `_` sigil for Swift —
    // `__swift5_types` mangled-name records often omit it (the
    // descriptor stores `$s...` directly). Try the demangler with
    // and without the prefix; the first non-empty / changed result
    // wins. Falls back to the raw input.
    let candidates = if raw.starts_with('_') {
        vec![raw.to_string()]
    } else if raw.starts_with('$') {
        vec![format!("_{raw}"), raw.to_string()]
    } else {
        vec![raw.to_string()]
    };
    for c in candidates {
        let out = demangle(&c);
        if !out.is_empty() && out != c {
            return out;
        }
    }
    raw.to_string()
}

/// One row of the Swift type viewer. Plain text rendering joins
/// chunk text in order; the UI walks `chunks` directly so each span
/// can carry its own colour + click handler.
#[derive(Clone, Debug)]
pub struct SwiftRow {
    pub chunks: Vec<Chunk>,
}

impl SwiftRow {
    fn new(chunks: Vec<Chunk>) -> Self {
        Self { chunks }
    }

    /// Flatten the chunk text — used for the body string the loader
    /// indexes for text-search / palette lookup.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for c in &self.chunks {
            s.push_str(&c.text);
        }
        s
    }
}

/// Build all rows for a Swift type summary. Includes a declaration
/// header line, an optional metadata-accessor comment, the field
/// list, and the vtable (for classes). Section headings are emitted
/// only when their section is non-empty.
pub fn render_type(t: &SwiftType) -> Vec<SwiftRow> {
    let mut rows = Vec::new();

    let pretty = pretty_swift_type_name(&t.mangled_name);
    let kw = match t.kind {
        SwiftTypeKind::Class => "class",
        SwiftTypeKind::Struct => "struct",
        SwiftTypeKind::Enum => "enum",
    };
    let header = vec![
        chunk(ChunkKind::Directive, kw),
        plain(" "),
        chunk(ChunkKind::Type, &pretty),
    ];
    rows.push(SwiftRow::new(header));

    if let Some(acc) = t.metadata_accessor_vaddr {
        rows.push(SwiftRow::new(vec![
            plain("  "),
            chunk(ChunkKind::Comment, "// metadata accessor @ "),
            Chunk {
                text: format!("0x{acc:x}"),
                kind: ChunkKind::Address,
                target: Some(acc),
                target_text: None,
            },
        ]));
    }

    if !t.fields.is_empty() {
        rows.push(section_heading("Fields"));
        for f in &t.fields {
            let type_pretty = pretty_swift_type_name(&f.mangled_type);
            let mut chunks = vec![
                plain("  "),
                chunk(ChunkKind::Plain, &f.name),
            ];
            if !type_pretty.is_empty() {
                chunks.push(plain(": "));
                chunks.push(chunk(ChunkKind::Type, &type_pretty));
            }
            rows.push(SwiftRow::new(chunks));
        }
    }

    if !t.vtable.is_empty() {
        rows.push(section_heading("V-table"));
        for (i, e) in t.vtable.iter().enumerate() {
            let mut chunks = vec![
                plain("  "),
                chunk(ChunkKind::MethodName, &format!("vtable[{i}]")),
                plain("  @ "),
                Chunk {
                    text: format!("0x{:x}", e.impl_vaddr),
                    kind: ChunkKind::Address,
                    target: Some(e.impl_vaddr),
                    target_text: None,
                },
            ];
            if e.flags != 0 {
                chunks.push(plain("  "));
                chunks.push(chunk(
                    ChunkKind::Comment,
                    &format!("// flags 0x{:x}", e.flags),
                ));
            }
            rows.push(SwiftRow::new(chunks));
        }
    }

    rows
}

fn section_heading(label: &str) -> SwiftRow {
    SwiftRow::new(vec![chunk(ChunkKind::Directive, &format!("// {label}"))])
}

fn plain(s: &str) -> Chunk {
    Chunk {
        text: s.to_string(),
        kind: ChunkKind::Plain,
        target: None,
        target_text: None,
    }
}

fn chunk(kind: ChunkKind, s: &str) -> Chunk {
    Chunk {
        text: s.to_string(),
        kind,
        target: None,
        target_text: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use armv8_encode::container::{SwiftField, SwiftVTableEntry};

    fn mk_class() -> SwiftType {
        SwiftType {
            mangled_name: "blackjack.ContentView".to_string(),
            descriptor_vaddr: 0x1000,
            kind: SwiftTypeKind::Class,
            parent_vaddr: None,
            fields: vec![
                SwiftField {
                    name: "count".to_string(),
                    mangled_type: "Si".to_string(),
                    flags: 0,
                },
                SwiftField {
                    name: "isReady".to_string(),
                    mangled_type: "Sb".to_string(),
                    flags: 0,
                },
            ],
            metadata_accessor_vaddr: Some(0x12345),
            vtable: vec![
                SwiftVTableEntry { impl_vaddr: 0x12abc, flags: 0 },
                SwiftVTableEntry { impl_vaddr: 0x12def, flags: 0 },
            ],
        }
    }

    #[test]
    fn pretty_swift_plain_passes_through() {
        // Already-pretty `module.Type` form survives unchanged.
        assert_eq!(
            pretty_swift_type_name("blackjack.ContentView"),
            "blackjack.ContentView"
        );
    }

    #[test]
    fn pretty_swift_empty_input() {
        assert_eq!(pretty_swift_type_name(""), "");
    }

    #[test]
    fn render_type_emits_header_fields_and_vtable() {
        let rows = render_type(&mk_class());
        // header + accessor comment + Fields heading + 2 fields
        // + V-table heading + 2 entries = 8.
        assert_eq!(rows.len(), 8);
        assert!(rows[0].text().starts_with("class blackjack.ContentView"));
        assert!(rows[1].text().contains("metadata accessor @ 0x12345"));
        assert!(rows.iter().any(|r| r.text().contains("count: Int")
            || r.text().contains("count: Si")
            || r.text().starts_with("  count")));
    }

    #[test]
    fn vtable_entry_carries_clickable_address() {
        let rows = render_type(&mk_class());
        let row = rows
            .iter()
            .find(|r| r.text().contains("vtable[0]"))
            .expect("vtable[0] row");
        let addr = row
            .chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Address)
            .expect("address chunk");
        assert_eq!(addr.target, Some(0x12abc));
        assert_eq!(addr.text, "0x12abc");
    }

    #[test]
    fn struct_uses_struct_keyword() {
        let mut t = mk_class();
        t.kind = SwiftTypeKind::Struct;
        t.vtable.clear();
        let rows = render_type(&t);
        assert!(rows[0].text().starts_with("struct "));
    }
}
