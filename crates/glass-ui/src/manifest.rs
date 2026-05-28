//! Pre-rendered row builders for the Manifest / Info.plist tabs.
//!
//! Both AndroidManifest (binary XML, via `smali`) and iOS Info.plist
//! viewers render the same underlying `ManifestRow` structure — a
//! depth + coloured token sequence — so the listing's virtualised
//! list and chunk renderer can be reused.

use std::sync::Arc;

use glass_arch_arm::{Chunk, ChunkKind};

/// One pre-rendered row of the manifest viewer. We flatten the tree
/// into row-per-line up front so the virtualized list can render
/// without recursing per-frame.
#[derive(Clone, Debug)]
pub struct ManifestRow {
    /// Tree depth — used for indentation.
    pub depth: usize,
    /// Coloured tokens for this line.
    pub chunks: Arc<Vec<Chunk>>,
}

/// Flatten a parsed AndroidManifest into one Vec<ManifestRow>. The
/// XML rendering follows the usual indented form:
///   <manifest android:foo="bar"
///             android:baz="qux">
///     <application ...>
///       <activity .../>
///     </application>
///   </manifest>
pub fn flatten_manifest(
    manifest: &smali::android::binary_xml::AndroidManifest,
) -> Vec<ManifestRow> {
    let mut rows = Vec::new();
    flatten_element(manifest.root(), 0, &mut rows);
    rows
}

fn flatten_element(
    elem: &smali::android::binary_xml::ManifestElement,
    depth: usize,
    rows: &mut Vec<ManifestRow>,
) {
    let mk =
        |text: String, kind: ChunkKind| Chunk { text, kind, target: None, target_text: None };

    let tag = qualified_tag(elem);
    let self_closing = elem.children.is_empty() && elem.text.is_none();

    if elem.attributes.is_empty() {
        let mut chunks = vec![mk("<".to_string(), ChunkKind::Punct)];
        chunks.push(mk(tag.clone(), ChunkKind::Directive));
        chunks.push(mk(if self_closing { "/>".to_string() } else { ">".to_string() }, ChunkKind::Punct));
        rows.push(ManifestRow { depth, chunks: Arc::new(chunks) });
    } else if elem.attributes.len() == 1 {
        let mut chunks = vec![mk("<".to_string(), ChunkKind::Punct)];
        chunks.push(mk(tag.clone(), ChunkKind::Directive));
        chunks.push(mk(" ".to_string(), ChunkKind::Plain));
        push_attribute(&elem.attributes[0], &mut chunks);
        chunks.push(mk(if self_closing { "/>".to_string() } else { ">".to_string() }, ChunkKind::Punct));
        rows.push(ManifestRow { depth, chunks: Arc::new(chunks) });
    } else {
        let mut first = vec![mk("<".to_string(), ChunkKind::Punct)];
        first.push(mk(tag.clone(), ChunkKind::Directive));
        first.push(mk(" ".to_string(), ChunkKind::Plain));
        push_attribute(&elem.attributes[0], &mut first);
        rows.push(ManifestRow { depth, chunks: Arc::new(first) });
        for (i, attr) in elem.attributes.iter().enumerate().skip(1) {
            let last = i == elem.attributes.len() - 1;
            let mut chunks = Vec::new();
            push_attribute(attr, &mut chunks);
            if last {
                chunks.push(mk(
                    if self_closing { "/>".to_string() } else { ">".to_string() },
                    ChunkKind::Punct,
                ));
            }
            rows.push(ManifestRow { depth: depth + 1, chunks: Arc::new(chunks) });
        }
    }

    if let Some(text) = elem.text.as_deref() {
        if !text.trim().is_empty() {
            let chunks = vec![mk(text.to_string(), ChunkKind::String)];
            rows.push(ManifestRow { depth: depth + 1, chunks: Arc::new(chunks) });
        }
    }

    for child in &elem.children {
        flatten_element(child, depth + 1, rows);
    }
    if !self_closing {
        let mut chunks = vec![mk("</".to_string(), ChunkKind::Punct)];
        chunks.push(mk(tag, ChunkKind::Directive));
        chunks.push(mk(">".to_string(), ChunkKind::Punct));
        rows.push(ManifestRow { depth, chunks: Arc::new(chunks) });
    }
}

fn qualified_tag(elem: &smali::android::binary_xml::ManifestElement) -> String {
    match elem.namespace_prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}:{}", elem.tag),
        _ => elem.tag.clone(),
    }
}

fn push_attribute(
    attr: &smali::android::binary_xml::ManifestAttribute,
    chunks: &mut Vec<Chunk>,
) {
    use smali::android::binary_xml::ManifestValue;

    let mk =
        |text: String, kind: ChunkKind| Chunk { text, kind, target: None, target_text: None };

    let name = match attr.namespace_prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}:{}", attr.name),
        _ => attr.name.clone(),
    };
    chunks.push(mk(name, ChunkKind::Modifier));
    chunks.push(mk("=".to_string(), ChunkKind::Punct));
    match &attr.value {
        ManifestValue::String(s) => {
            chunks.push(mk(format!("\"{s}\""), ChunkKind::String));
        }
        ManifestValue::Boolean(b) => {
            chunks.push(mk(format!("\"{b}\""), ChunkKind::Immediate));
        }
        ManifestValue::Integer(n) => {
            chunks.push(mk(format!("\"{n}\""), ChunkKind::Immediate));
        }
        ManifestValue::Hex(h) => {
            chunks.push(mk(format!("\"0x{h:x}\""), ChunkKind::Immediate));
        }
        ManifestValue::Reference(r) => {
            chunks.push(mk(format!("\"@0x{r:x}\""), ChunkKind::Type));
        }
    }
}

// ---- iOS Info.plist --------------------------------------------------------

/// Render an `Info.plist` into the same depth-indented `ManifestRow`
/// stream that the XML viewer consumes. The output reads like a
/// pretty-printed plist (`<key>`/`<string>` etc.).
pub fn flatten_info_plist(info: &glass_mobile::InfoPlist) -> Vec<ManifestRow> {
    let mk = |text: String, kind: ChunkKind| Chunk {
        text,
        kind,
        target: None,
        target_text: None,
    };
    let mut rows = Vec::new();
    rows.push(ManifestRow {
        depth: 0,
        chunks: Arc::new(vec![
            mk("<".into(), ChunkKind::Punct),
            mk("plist".into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]),
    });
    rows.push(ManifestRow {
        depth: 1,
        chunks: Arc::new(vec![
            mk("<".into(), ChunkKind::Punct),
            mk("dict".into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]),
    });
    if let Some(v) = info.extras.as_ref() {
        flatten_plist_value(v, 2, &mut rows);
    }
    rows.push(ManifestRow {
        depth: 1,
        chunks: Arc::new(vec![
            mk("</".into(), ChunkKind::Punct),
            mk("dict".into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]),
    });
    rows.push(ManifestRow {
        depth: 0,
        chunks: Arc::new(vec![
            mk("</".into(), ChunkKind::Punct),
            mk("plist".into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]),
    });
    rows
}

fn flatten_plist_value(
    value: &plist::Value,
    depth: usize,
    rows: &mut Vec<ManifestRow>,
) {
    let mk = |text: String, kind: ChunkKind| Chunk {
        text,
        kind,
        target: None,
        target_text: None,
    };
    let scalar = |tag: &str, raw: String, kind: ChunkKind| {
        vec![
            mk("<".into(), ChunkKind::Punct),
            mk(tag.into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
            mk(raw, kind),
            mk("</".into(), ChunkKind::Punct),
            mk(tag.into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]
    };

    match value {
        plist::Value::Dictionary(dict) => {
            for (key, child) in dict.iter() {
                rows.push(ManifestRow {
                    depth,
                    chunks: Arc::new(vec![
                        mk("<".into(), ChunkKind::Punct),
                        mk("key".into(), ChunkKind::Directive),
                        mk(">".into(), ChunkKind::Punct),
                        mk(key.to_string(), ChunkKind::String),
                        mk("</".into(), ChunkKind::Punct),
                        mk("key".into(), ChunkKind::Directive),
                        mk(">".into(), ChunkKind::Punct),
                    ]),
                });
                flatten_plist_value(child, depth, rows);
            }
        }
        plist::Value::Array(arr) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(vec![
                    mk("<".into(), ChunkKind::Punct),
                    mk("array".into(), ChunkKind::Directive),
                    mk(">".into(), ChunkKind::Punct),
                ]),
            });
            for item in arr {
                flatten_plist_value(item, depth + 1, rows);
            }
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(vec![
                    mk("</".into(), ChunkKind::Punct),
                    mk("array".into(), ChunkKind::Directive),
                    mk(">".into(), ChunkKind::Punct),
                ]),
            });
        }
        plist::Value::String(s) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("string", s.clone(), ChunkKind::String)),
            });
        }
        plist::Value::Integer(n) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("integer", n.to_string(), ChunkKind::Modifier)),
            });
        }
        plist::Value::Real(r) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("real", r.to_string(), ChunkKind::Modifier)),
            });
        }
        plist::Value::Boolean(b) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(vec![
                    mk("<".into(), ChunkKind::Punct),
                    mk(if *b { "true" } else { "false" }.into(), ChunkKind::Directive),
                    mk("/>".into(), ChunkKind::Punct),
                ]),
            });
        }
        plist::Value::Date(d) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("date", format!("{d:?}"), ChunkKind::String)),
            });
        }
        plist::Value::Data(bytes) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar(
                    "data",
                    format!("[{} bytes]", bytes.len()),
                    ChunkKind::Comment,
                )),
            });
        }
        plist::Value::Uid(uid) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("uid", format!("{uid:?}"), ChunkKind::Modifier)),
            });
        }
        _ => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(vec![mk("<unknown/>".into(), ChunkKind::Comment)]),
            });
        }
    }
}
