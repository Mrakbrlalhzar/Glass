//! Rendered text + typed-chunk row model for an Objective-C class.
//!
//! Walks an [`ObjCClass`] / [`ObjCCategory`] (from
//! `armv8_encode::container`) and produces a sequence of rows the
//! glass-ui ObjC tab renders. Each row carries `Chunk`s in the same
//! shape the disassembly listing uses, so `ChunkKind::Address`
//! chunks with `target: Some(addr)` become clickable jump-to-listing
//! links via the existing UI wiring.
//!
//! The rendering is biased toward readability — it does not aim to
//! reproduce valid `.h` syntax. The intent is "show the user the
//! methods and ivars at a glance, with addresses they can jump
//! into".

use armv8_encode::container::{ObjCCategory, ObjCClass, ObjCIvar, ObjCMethod, ObjCProperty};

use crate::format::{Chunk, ChunkKind};
use crate::symbol_map::demangle;

/// Best-effort demangle for an ObjC class name. Plain ObjC
/// classes (`NSString`, `UIViewController`, `MyAppDelegate`)
/// pass through untouched; Swift classes registered with the
/// ObjC runtime carry mangled names (`_TtC...` / `_$s...C`)
/// that the upstream `symbolic-demangle` knows how to unwind.
pub fn pretty_class_name(raw: &str) -> String {
    if !raw.starts_with('_') {
        return raw.to_string();
    }
    let p = demangle(raw);
    if p.is_empty() || p == raw {
        raw.to_string()
    } else {
        p
    }
}

/// One row of the ObjC class viewer. Plain text rendering joins
/// chunk text in order; the UI walks `chunks` directly so each
/// span can carry its own colour + click handler.
#[derive(Clone, Debug)]
pub struct ObjCRow {
    pub chunks: Vec<Chunk>,
}

impl ObjCRow {
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

/// Build all rows for a class summary. Includes a declaration line,
/// adopted protocols (if any), instance methods, class methods,
/// ivars and properties. Headings are emitted only when their
/// section is non-empty.
pub fn render_class(class: &ObjCClass) -> Vec<ObjCRow> {
    let mut rows = Vec::new();

    // `@interface Foo : Super <Protocols> { ... }` style header.
    // Demangle Swift mangled class names so users see `MyApp.MyClass`
    // instead of `_$s5MyApp7MyClassC`.
    let pretty = pretty_class_name(&class.name);
    let mut header = vec![plain("@interface "), chunk(ChunkKind::Type, &pretty)];
    if let Some(sup) = class.superclass_name.as_deref() {
        let sup_pretty = pretty_class_name(sup);
        header.push(plain(" : "));
        header.push(chunk(ChunkKind::Type, &sup_pretty));
    }
    rows.push(ObjCRow::new(header));
    rows.push(ObjCRow::new(vec![
        plain("  // size 0x"),
        chunk(ChunkKind::Immediate, &format!("{:x}", class.instance_size)),
        plain(", flags 0x"),
        chunk(ChunkKind::Immediate, &format!("{:x}", class.flags)),
    ]));

    if !class.instance_methods.is_empty() {
        rows.push(section_heading("Instance methods"));
        for m in &class.instance_methods {
            rows.push(render_method_row(&pretty, m, MethodKind::Instance));
        }
    }
    if !class.class_methods.is_empty() {
        rows.push(section_heading("Class methods"));
        for m in &class.class_methods {
            rows.push(render_method_row(&pretty, m, MethodKind::Class));
        }
    }
    if !class.ivars.is_empty() {
        rows.push(section_heading("Ivars"));
        for ivar in &class.ivars {
            rows.push(render_ivar_row(ivar));
        }
    }
    if !class.properties.is_empty() {
        rows.push(section_heading("Properties"));
        for p in &class.properties {
            rows.push(render_property_row(p));
        }
    }
    if !class.adopted_protocols.is_empty() {
        rows.push(section_heading("Adopted protocols"));
        for proto_addr in &class.adopted_protocols {
            rows.push(ObjCRow::new(vec![
                plain("  "),
                chunk(
                    ChunkKind::Immediate,
                    &format!("<protocol @ 0x{proto_addr:x}>"),
                ),
            ]));
        }
    }
    rows.push(ObjCRow::new(vec![plain("@end")]));
    rows
}

/// Render a category. Methods are nested under the base class with
/// the category name in parentheses (`-[NSString(MyExt) lowercased]`).
pub fn render_category(cat: &ObjCCategory) -> Vec<ObjCRow> {
    let mut rows = Vec::new();
    let base_raw = cat.class_name.as_deref().unwrap_or("?");
    let base = pretty_class_name(base_raw);
    let display = format!("{base}({})", cat.name);

    let header = vec![
        plain("@interface "),
        chunk(ChunkKind::Type, &base),
        plain(" ("),
        chunk(ChunkKind::Type, &cat.name),
        plain(")"),
    ];
    rows.push(ObjCRow::new(header));

    if !cat.instance_methods.is_empty() {
        rows.push(section_heading("Instance methods"));
        for m in &cat.instance_methods {
            rows.push(render_method_row(&display, m, MethodKind::Instance));
        }
    }
    if !cat.class_methods.is_empty() {
        rows.push(section_heading("Class methods"));
        for m in &cat.class_methods {
            rows.push(render_method_row(&display, m, MethodKind::Class));
        }
    }
    if !cat.instance_properties.is_empty() || !cat.class_properties.is_empty() {
        rows.push(section_heading("Properties"));
        for p in &cat.instance_properties {
            rows.push(render_property_row(p));
        }
        for p in &cat.class_properties {
            rows.push(render_property_row(p));
        }
    }
    rows.push(ObjCRow::new(vec![plain("@end")]));
    rows
}

#[derive(Clone, Copy)]
enum MethodKind {
    Instance,
    Class,
}

impl MethodKind {
    fn sigil(self) -> &'static str {
        match self {
            MethodKind::Instance => "-",
            MethodKind::Class => "+",
        }
    }
}

fn render_method_row(class_name: &str, m: &ObjCMethod, kind: MethodKind) -> ObjCRow {
    let mut chunks = vec![
        plain("  "),
        plain(kind.sigil()),
        plain("["),
        chunk(ChunkKind::Type, class_name),
        plain(" "),
        chunk(ChunkKind::MethodName, &m.name),
        plain("]"),
    ];
    if !m.types.is_empty() {
        chunks.push(plain("  "));
        chunks.push(chunk(ChunkKind::Comment, &format!("// {}", m.types)));
        // separator so the optional address sits after the type
        // comment when present
        chunks.push(plain(""));
    }
    if let Some(addr) = m.imp {
        chunks.push(plain("  @ "));
        chunks.push(Chunk {
            text: format!("0x{addr:x}"),
            kind: ChunkKind::Address,
            target: Some(addr),
            target_text: None,
        });
    }
    ObjCRow::new(chunks)
}

fn render_ivar_row(ivar: &ObjCIvar) -> ObjCRow {
    ObjCRow::new(vec![
        plain("  "),
        chunk(ChunkKind::Plain, &ivar.name),
        plain(": "),
        chunk(ChunkKind::Type, &ivar.types),
        plain("  @ +0x"),
        chunk(ChunkKind::Immediate, &format!("{:x}", ivar.offset)),
        plain(" (size 0x"),
        chunk(ChunkKind::Immediate, &format!("{:x}", ivar.size)),
        plain(")"),
    ])
}

fn render_property_row(p: &ObjCProperty) -> ObjCRow {
    ObjCRow::new(vec![
        plain("  @property ("),
        chunk(ChunkKind::Modifier, &p.attributes),
        plain(") "),
        chunk(ChunkKind::Plain, &p.name),
    ])
}

fn section_heading(label: &str) -> ObjCRow {
    ObjCRow::new(vec![chunk(ChunkKind::Directive, &format!("// {label}"))])
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

    fn mk_class() -> ObjCClass {
        ObjCClass {
            name: "Foo".to_string(),
            vaddr: 0x1000,
            metaclass_vaddr: None,
            superclass_vaddr: None,
            superclass_name: Some("NSObject".to_string()),
            flags: 0,
            instance_start: 8,
            instance_size: 16,
            instance_methods: vec![ObjCMethod {
                name: "init".to_string(),
                types: "@16@0:8".to_string(),
                imp: Some(0x1234),
            }],
            class_methods: vec![ObjCMethod {
                name: "alloc".to_string(),
                types: "@16@0:8".to_string(),
                imp: Some(0x5678),
            }],
            ivars: vec![ObjCIvar {
                name: "_count".to_string(),
                types: "i".to_string(),
                offset_ptr_vaddr: 0x2000,
                offset: 8,
                alignment: 2,
                size: 4,
            }],
            properties: vec![ObjCProperty {
                name: "count".to_string(),
                attributes: "Ti,N".to_string(),
            }],
            adopted_protocols: Vec::new(),
        }
    }

    #[test]
    fn render_class_emits_header_methods_ivars_and_properties() {
        let rows = render_class(&mk_class());
        // Header (2 rows) + instance methods heading + 1 method
        // + class methods heading + 1 method + ivars heading + 1
        // ivar + properties heading + 1 property + `@end`.
        assert_eq!(rows.len(), 11);
        assert!(rows[0].text().starts_with("@interface Foo"));
        assert!(rows.last().unwrap().text() == "@end");
    }

    #[test]
    fn instance_method_carries_clickable_address() {
        let rows = render_class(&mk_class());
        let method_row = rows
            .iter()
            .find(|r| r.text().contains("-[Foo init]"))
            .expect("instance method row present");
        let addr_chunk = method_row
            .chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Address)
            .expect("clickable address chunk present");
        assert_eq!(addr_chunk.target, Some(0x1234));
        assert_eq!(addr_chunk.text, "0x1234");
    }

    #[test]
    fn class_method_uses_plus_sigil() {
        let rows = render_class(&mk_class());
        assert!(rows.iter().any(|r| r.text().contains("+[Foo alloc]")));
    }

    #[test]
    fn ivar_row_shows_offset_and_size() {
        let rows = render_class(&mk_class());
        let row = rows
            .iter()
            .find(|r| r.text().contains("_count"))
            .expect("ivar row");
        let text = row.text();
        assert!(text.contains("@ +0x8"), "got: {text}");
        assert!(text.contains("size 0x4"), "got: {text}");
    }

    #[test]
    fn category_uses_paren_form() {
        let cat = ObjCCategory {
            name: "MyExt".to_string(),
            vaddr: 0,
            class_vaddr: None,
            class_name: Some("NSString".to_string()),
            instance_methods: vec![ObjCMethod {
                name: "lowercased".to_string(),
                types: "@16@0:8".to_string(),
                imp: Some(0x4000),
            }],
            class_methods: Vec::new(),
            protocols: Vec::new(),
            instance_properties: Vec::new(),
            class_properties: Vec::new(),
        };
        let rows = render_category(&cat);
        assert!(rows
            .iter()
            .any(|r| r.text().contains("-[NSString(MyExt) lowercased]")));
    }
}
