//! ObjC + Swift type metadata verbs — `types` (list) and `type` (detail).
//!
//! These wrap the readers in `armv8_encode::container`
//! (`read_objc_metadata`, `read_swift_metadata`) and project the
//! results into JSON-friendly shapes. iOS-only; APKs / ELFs return
//! empty.

use anyhow::{Context, Result};
use armv8_encode::container::{
    read_objc_metadata, read_swift_metadata, ObjCCategory, ObjCClass, ObjCIvar,
    ObjCMethod, ObjCMetadata, ObjCProperty, ObjCProtocol, SwiftMetadata, SwiftType,
    SwiftTypeKind,
};
use glass_arch_arm::objc_format::pretty_class_name;
use glass_arch_arm::swift_format::pretty_swift_type_name;
use serde::Serialize;

use crate::bundle::{Bundle, ParsedArtifact};

// ---------------------------------------------------------------------------
// Listing types
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug, Clone)]
pub struct TypesResult {
    pub total: usize,
    pub shown: usize,
    pub entries: Vec<TypeEntry>,
}

#[derive(Serialize, Debug, Clone)]
pub struct TypeEntry {
    pub kind: TypeKind,
    /// Demangled / pretty form (`blackjack.ContentView`,
    /// `NSString(MyExt)`).
    pub name: String,
    /// Raw / mangled persistence key (`_$s9blackjack11ContentViewC`).
    pub raw_name: String,
    /// Artifact label.
    pub artifact: String,
    /// Hex vmaddr — descriptor for Swift, class vaddr for ObjC.
    pub vaddr: String,
    /// ObjC: `instance_methods + class_methods`. Swift: `vtable.len()`.
    pub method_count: usize,
    /// ObjC: ivars. Swift: fields.
    pub field_count: usize,
    /// Pretty base class name for categories; `None` otherwise.
    pub category_for: Option<String>,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TypeKind {
    ObjcClass,
    ObjcCategory,
    SwiftClass,
    SwiftStruct,
    SwiftEnum,
}

impl TypeKind {
    /// Parse the CLI / MCP string form into a kind filter.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "objc-class" => Some(TypeKind::ObjcClass),
            "objc-category" => Some(TypeKind::ObjcCategory),
            "swift-class" => Some(TypeKind::SwiftClass),
            "swift-struct" => Some(TypeKind::SwiftStruct),
            "swift-enum" => Some(TypeKind::SwiftEnum),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Detail types
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TypeDetail {
    ObjcClass(ObjcClassDetail),
    ObjcCategory(ObjcCategoryDetail),
    SwiftClass(SwiftTypeDetail),
    SwiftStruct(SwiftTypeDetail),
    SwiftEnum(SwiftTypeDetail),
}

#[derive(Serialize, Debug, Clone)]
pub struct ObjcClassDetail {
    pub name: String,
    pub raw_name: String,
    pub artifact: String,
    pub vaddr: String,
    pub superclass: Option<String>,
    pub flags: u32,
    pub instance_size: u32,
    pub instance_methods: Vec<ObjcMethodEntry>,
    pub class_methods: Vec<ObjcMethodEntry>,
    pub ivars: Vec<ObjcIvarEntry>,
    pub properties: Vec<ObjcPropertyEntry>,
    pub adopted_protocols: Vec<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ObjcCategoryDetail {
    pub name: String,
    pub raw_name: String,
    pub artifact: String,
    pub category_for: String,
    pub vaddr: String,
    pub instance_methods: Vec<ObjcMethodEntry>,
    pub class_methods: Vec<ObjcMethodEntry>,
    pub instance_properties: Vec<ObjcPropertyEntry>,
    pub class_properties: Vec<ObjcPropertyEntry>,
    pub protocols: Vec<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct SwiftTypeDetail {
    pub name: String,
    pub raw_name: String,
    pub artifact: String,
    pub descriptor_vaddr: String,
    pub parent_vaddr: Option<String>,
    pub metadata_accessor_vaddr: Option<String>,
    pub fields: Vec<SwiftFieldEntry>,
    pub vtable: Vec<SwiftVtableEntryDetail>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ObjcMethodEntry {
    pub name: String,
    pub types: String,
    pub imp_vaddr: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ObjcIvarEntry {
    pub name: String,
    pub type_enc: String,
    pub offset: String,
    pub size: u32,
}

#[derive(Serialize, Debug, Clone)]
pub struct ObjcPropertyEntry {
    pub name: String,
    pub attributes: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct SwiftFieldEntry {
    pub name: String,
    /// Pretty / demangled type. Empty when the demangler bailed
    /// (typically a control-byte-prefixed `__swift5_typeref` ref).
    pub type_pretty: String,
    pub raw_type: String,
    pub flags: u32,
}

#[derive(Serialize, Debug, Clone)]
pub struct SwiftVtableEntryDetail {
    pub index: usize,
    pub impl_vaddr: String,
    pub flags: u32,
}

// ---------------------------------------------------------------------------
// Bundle impls
// ---------------------------------------------------------------------------

impl Bundle {
    /// List ObjC + Swift class-like entities across the bundle's
    /// Mach-O artifacts. APK / ELF artifacts contribute nothing.
    pub fn types(
        &self,
        artifact_ref: Option<&str>,
        kind_filter: Option<TypeKind>,
        package_prefix: Option<&str>,
        limit: Option<usize>,
    ) -> Result<TypesResult> {
        let mut entries: Vec<TypeEntry> = Vec::new();
        for a in &self.artifacts {
            if let Some(needle) = artifact_ref {
                if a.label != needle && !a.id.to_string().starts_with(needle) {
                    continue;
                }
            }
            collect_types_for_artifact(a, &mut entries);
        }
        // Filter by package prefix (pretty name) + kind.
        entries.retain(|e| {
            kind_filter.is_none_or(|k| e.kind == k)
                && package_prefix.is_none_or(|p| e.name.starts_with(p))
        });
        entries.sort_by(|x, y| {
            x.artifact
                .cmp(&y.artifact)
                .then_with(|| kind_order(x.kind).cmp(&kind_order(y.kind)))
                .then_with(|| x.name.cmp(&y.name))
        });
        let total = entries.len();
        let cap = limit.unwrap_or(usize::MAX);
        if entries.len() > cap {
            entries.truncate(cap);
        }
        let shown = entries.len();
        Ok(TypesResult { total, shown, entries })
    }

    /// Detail view for one class / type by name. Looks up by pretty
    /// form first, falling back to raw form. `raw` skips all
    /// pretty-name conversion in the response.
    pub fn type_detail(
        &self,
        artifact_ref: &str,
        name: &str,
        raw: bool,
    ) -> Result<TypeDetail> {
        let a = self
            .artifacts
            .iter()
            .find(|a| {
                a.label == artifact_ref
                    || a.id.to_string().starts_with(artifact_ref)
            })
            .with_context(|| {
                format!("no artifact matches {artifact_ref:?}")
            })?;
        let (objc, swift) = read_artifact_metadata(a);

        // Pass 1: pretty-name match across ObjC classes, categories, Swift types.
        if let Some(o) = &objc {
            for c in &o.classes {
                if pretty_class_name(&c.name) == name {
                    return Ok(TypeDetail::ObjcClass(make_objc_class_detail(
                        c, &a.label, raw,
                    )));
                }
            }
            for cat in &o.categories {
                if pretty_category_name(cat) == name {
                    return Ok(TypeDetail::ObjcCategory(make_objc_category_detail(
                        cat, &a.label, raw,
                    )));
                }
            }
        }
        if let Some(s) = &swift {
            for t in &s.types {
                if pretty_swift_type_name(&t.mangled_name) == name {
                    return Ok(swift_detail_variant(t, &a.label, raw));
                }
            }
        }
        // Pass 2: raw-name match.
        if let Some(o) = &objc {
            for c in &o.classes {
                if c.name == name {
                    return Ok(TypeDetail::ObjcClass(make_objc_class_detail(
                        c, &a.label, raw,
                    )));
                }
            }
            for cat in &o.categories {
                if raw_category_name(cat) == name {
                    return Ok(TypeDetail::ObjcCategory(make_objc_category_detail(
                        cat, &a.label, raw,
                    )));
                }
            }
        }
        if let Some(s) = &swift {
            for t in &s.types {
                if t.mangled_name == name {
                    return Ok(swift_detail_variant(t, &a.label, raw));
                }
            }
        }
        anyhow::bail!(
            "no class/type matches {name:?} in artifact {artifact_ref:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Collection / shaping
// ---------------------------------------------------------------------------

fn kind_order(k: TypeKind) -> u8 {
    match k {
        TypeKind::ObjcClass => 0,
        TypeKind::ObjcCategory => 1,
        TypeKind::SwiftClass => 2,
        TypeKind::SwiftStruct => 3,
        TypeKind::SwiftEnum => 4,
    }
}

fn read_artifact_metadata(
    a: &ParsedArtifact,
) -> (Option<ObjCMetadata>, Option<SwiftMetadata>) {
    let image = match a.binary.container.macho_image.as_ref() {
        Some(img) => img,
        None => return (None, None),
    };
    let objc = match read_objc_metadata(image) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::trace!(
                artifact = %a.label,
                error = ?e,
                "read_objc_metadata failed (non-fatal)"
            );
            None
        }
    };
    let swift = match read_swift_metadata(image) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::trace!(
                artifact = %a.label,
                error = ?e,
                "read_swift_metadata failed (non-fatal)"
            );
            None
        }
    };
    (objc, swift)
}

fn collect_types_for_artifact(a: &ParsedArtifact, out: &mut Vec<TypeEntry>) {
    let (objc, swift) = read_artifact_metadata(a);
    if let Some(o) = objc {
        for c in &o.classes {
            out.push(TypeEntry {
                kind: TypeKind::ObjcClass,
                name: pretty_class_name(&c.name),
                raw_name: c.name.clone(),
                artifact: a.label.clone(),
                vaddr: format!("0x{:x}", c.vaddr),
                method_count: c.instance_methods.len() + c.class_methods.len(),
                field_count: c.ivars.len(),
                category_for: None,
            });
        }
        for cat in &o.categories {
            let base_pretty = cat
                .class_name
                .as_deref()
                .map(pretty_class_name)
                .unwrap_or_else(|| "?".to_string());
            out.push(TypeEntry {
                kind: TypeKind::ObjcCategory,
                name: pretty_category_name(cat),
                raw_name: raw_category_name(cat),
                artifact: a.label.clone(),
                vaddr: format!("0x{:x}", cat.vaddr),
                method_count: cat.instance_methods.len() + cat.class_methods.len(),
                field_count: 0,
                category_for: Some(base_pretty),
            });
        }
    }
    if let Some(s) = swift {
        for t in &s.types {
            out.push(TypeEntry {
                kind: swift_kind(t.kind),
                name: pretty_swift_type_name(&t.mangled_name),
                raw_name: t.mangled_name.clone(),
                artifact: a.label.clone(),
                vaddr: format!("0x{:x}", t.descriptor_vaddr),
                method_count: t.vtable.len(),
                field_count: t.fields.len(),
                category_for: None,
            });
        }
    }
}

fn swift_kind(k: SwiftTypeKind) -> TypeKind {
    match k {
        SwiftTypeKind::Class => TypeKind::SwiftClass,
        SwiftTypeKind::Struct => TypeKind::SwiftStruct,
        SwiftTypeKind::Enum => TypeKind::SwiftEnum,
    }
}

fn pretty_category_name(cat: &ObjCCategory) -> String {
    let base = cat
        .class_name
        .as_deref()
        .map(pretty_class_name)
        .unwrap_or_else(|| "?".to_string());
    format!("{base}({})", cat.name)
}

fn raw_category_name(cat: &ObjCCategory) -> String {
    let base = cat.class_name.clone().unwrap_or_else(|| "?".to_string());
    format!("{base}({})", cat.name)
}

// ---------------------------------------------------------------------------
// Detail builders
// ---------------------------------------------------------------------------

fn make_objc_class_detail(
    c: &ObjCClass,
    artifact_label: &str,
    raw: bool,
) -> ObjcClassDetail {
    let superclass = c.superclass_name.clone().map(|n| {
        if raw {
            n
        } else {
            pretty_class_name(&n)
        }
    });
    ObjcClassDetail {
        name: if raw {
            c.name.clone()
        } else {
            pretty_class_name(&c.name)
        },
        raw_name: c.name.clone(),
        artifact: artifact_label.to_string(),
        vaddr: format!("0x{:x}", c.vaddr),
        superclass,
        flags: c.flags,
        instance_size: c.instance_size,
        instance_methods: c.instance_methods.iter().map(objc_method_entry).collect(),
        class_methods: c.class_methods.iter().map(objc_method_entry).collect(),
        ivars: c.ivars.iter().map(objc_ivar_entry).collect(),
        properties: c.properties.iter().map(objc_property_entry).collect(),
        adopted_protocols: c
            .adopted_protocols
            .iter()
            .map(|v| format!("0x{v:x}"))
            .collect(),
    }
}

fn make_objc_category_detail(
    cat: &ObjCCategory,
    artifact_label: &str,
    raw: bool,
) -> ObjcCategoryDetail {
    let base_raw = cat.class_name.clone().unwrap_or_else(|| "?".to_string());
    let base_pretty = if raw {
        base_raw.clone()
    } else {
        pretty_class_name(&base_raw)
    };
    let name = if raw {
        format!("{base_raw}({})", cat.name)
    } else {
        format!("{base_pretty}({})", cat.name)
    };
    ObjcCategoryDetail {
        name,
        raw_name: format!("{base_raw}({})", cat.name),
        artifact: artifact_label.to_string(),
        category_for: base_pretty,
        vaddr: format!("0x{:x}", cat.vaddr),
        instance_methods: cat.instance_methods.iter().map(objc_method_entry).collect(),
        class_methods: cat.class_methods.iter().map(objc_method_entry).collect(),
        instance_properties: cat
            .instance_properties
            .iter()
            .map(objc_property_entry)
            .collect(),
        class_properties: cat
            .class_properties
            .iter()
            .map(objc_property_entry)
            .collect(),
        protocols: cat.protocols.iter().map(|v| format!("0x{v:x}")).collect(),
    }
}

fn swift_detail_variant(t: &SwiftType, artifact_label: &str, raw: bool) -> TypeDetail {
    let d = make_swift_type_detail(t, artifact_label, raw);
    match t.kind {
        SwiftTypeKind::Class => TypeDetail::SwiftClass(d),
        SwiftTypeKind::Struct => TypeDetail::SwiftStruct(d),
        SwiftTypeKind::Enum => TypeDetail::SwiftEnum(d),
    }
}

fn make_swift_type_detail(
    t: &SwiftType,
    artifact_label: &str,
    raw: bool,
) -> SwiftTypeDetail {
    SwiftTypeDetail {
        name: if raw {
            t.mangled_name.clone()
        } else {
            pretty_swift_type_name(&t.mangled_name)
        },
        raw_name: t.mangled_name.clone(),
        artifact: artifact_label.to_string(),
        descriptor_vaddr: format!("0x{:x}", t.descriptor_vaddr),
        parent_vaddr: t.parent_vaddr.map(|v| format!("0x{v:x}")),
        metadata_accessor_vaddr: t.metadata_accessor_vaddr.map(|v| format!("0x{v:x}")),
        fields: t
            .fields
            .iter()
            .map(|f| SwiftFieldEntry {
                name: f.name.clone(),
                type_pretty: if raw {
                    String::new()
                } else {
                    pretty_swift_type_name(&f.mangled_type)
                },
                raw_type: f.mangled_type.clone(),
                flags: f.flags,
            })
            .collect(),
        vtable: t
            .vtable
            .iter()
            .enumerate()
            .map(|(i, e)| SwiftVtableEntryDetail {
                index: i,
                impl_vaddr: format!("0x{:x}", e.impl_vaddr),
                flags: e.flags,
            })
            .collect(),
    }
}

fn objc_method_entry(m: &ObjCMethod) -> ObjcMethodEntry {
    ObjcMethodEntry {
        name: m.name.clone(),
        types: m.types.clone(),
        imp_vaddr: m.imp.map(|v| format!("0x{v:x}")),
    }
}

fn objc_ivar_entry(i: &ObjCIvar) -> ObjcIvarEntry {
    ObjcIvarEntry {
        name: i.name.clone(),
        type_enc: i.types.clone(),
        offset: format!("0x{:x}", i.offset),
        size: i.size,
    }
}

fn objc_property_entry(p: &ObjCProperty) -> ObjcPropertyEntry {
    ObjcPropertyEntry {
        name: p.name.clone(),
        attributes: p.attributes.clone(),
    }
}

// `ObjCProtocol` referenced in signatures only via `ObjCMetadata`;
// silence the unused-import diagnostic.
#[allow(dead_code)]
fn _protocol_marker(_p: &ObjCProtocol) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_kind_parse_roundtrip() {
        assert_eq!(TypeKind::parse("objc-class"), Some(TypeKind::ObjcClass));
        assert_eq!(TypeKind::parse("objc-category"), Some(TypeKind::ObjcCategory));
        assert_eq!(TypeKind::parse("swift-class"), Some(TypeKind::SwiftClass));
        assert_eq!(TypeKind::parse("swift-struct"), Some(TypeKind::SwiftStruct));
        assert_eq!(TypeKind::parse("swift-enum"), Some(TypeKind::SwiftEnum));
        assert_eq!(TypeKind::parse("nonsense"), None);
    }

    #[test]
    fn kind_order_is_total() {
        let order = [
            TypeKind::ObjcClass,
            TypeKind::ObjcCategory,
            TypeKind::SwiftClass,
            TypeKind::SwiftStruct,
            TypeKind::SwiftEnum,
        ];
        for w in order.windows(2) {
            assert!(kind_order(w[0]) < kind_order(w[1]));
        }
    }

    #[test]
    fn swift_kind_mapping() {
        assert_eq!(swift_kind(SwiftTypeKind::Class), TypeKind::SwiftClass);
        assert_eq!(swift_kind(SwiftTypeKind::Struct), TypeKind::SwiftStruct);
        assert_eq!(swift_kind(SwiftTypeKind::Enum), TypeKind::SwiftEnum);
    }

    #[test]
    fn category_name_helpers() {
        let cat = ObjCCategory {
            name: "MyExt".to_string(),
            vaddr: 0x1000,
            class_vaddr: None,
            class_name: Some("NSString".to_string()),
            instance_methods: vec![],
            class_methods: vec![],
            protocols: vec![],
            instance_properties: vec![],
            class_properties: vec![],
        };
        assert_eq!(raw_category_name(&cat), "NSString(MyExt)");
        // NSString isn't mangled, so pretty == raw here.
        assert_eq!(pretty_category_name(&cat), "NSString(MyExt)");
    }
}
