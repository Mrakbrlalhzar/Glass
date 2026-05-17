//! DEX / smali verbs — classes, methods, fields, smali bodies, call sites.
//!
//! These all operate on the pre-lifted `SmaliClass` list collected
//! when the APK was opened (see [`open_apk`]). IPA / Native bundles
//! have no DEX data, so every verb here returns an empty result.

use anyhow::{Context, Result};
use serde::Serialize;
use smali::smali_ops::{DexOp, MethodRef};
use smali::types::{SmaliClass, SmaliMethod, SmaliOp};

use crate::bundle::Bundle;

#[derive(Serialize, Debug, Clone)]
pub struct ClassListing {
    pub total: usize,
    pub shown: usize,
    pub classes: Vec<ClassInfo>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ClassInfo {
    /// JNI form (e.g. `Lcom/example/Foo;`).
    pub jni: String,
    /// Java form (e.g. `com.example.Foo`).
    pub java: String,
    pub super_class: String,
    pub field_count: usize,
    pub method_count: usize,
}

#[derive(Serialize, Debug, Clone)]
pub struct SmaliBody {
    pub class: String,
    pub smali: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct MethodListing {
    pub class: String,
    pub methods: Vec<MethodInfo>,
}

#[derive(Serialize, Debug, Clone)]
pub struct MethodInfo {
    pub name: String,
    /// JNI descriptor of the method, e.g. `(Ljava/lang/String;)V`.
    pub descriptor: String,
    pub constructor: bool,
    pub modifiers: Vec<String>,
    pub op_count: usize,
}

#[derive(Serialize, Debug, Clone)]
pub struct FieldListing {
    pub class: String,
    pub fields: Vec<FieldInfo>,
}

#[derive(Serialize, Debug, Clone)]
pub struct FieldInfo {
    pub name: String,
    pub type_jni: String,
    pub modifiers: Vec<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct MethodCallsResult {
    pub class: String,
    pub method: String,
    pub descriptor: String,
    pub calls: Vec<MethodCallSite>,
}

#[derive(Serialize, Debug, Clone)]
pub struct MethodCallSite {
    pub kind: String,
    pub target_class: String,
    pub target_method: String,
    pub target_descriptor: String,
}

impl Bundle {
    /// All DEX classes, optionally filtered by JNI / Java prefix.
    pub fn classes(&self, prefix: Option<&str>) -> ClassListing {
        let total = self.dex_classes.len();
        let mut out: Vec<ClassInfo> = self
            .dex_classes
            .iter()
            .filter(|c| match prefix {
                Some(p) => {
                    let jni = c.name.as_jni_type();
                    let java = c.name.as_java_type();
                    jni.starts_with(p) || java.starts_with(p)
                }
                None => true,
            })
            .map(class_info)
            .collect();
        out.sort_by(|a, b| a.jni.cmp(&b.jni));
        let shown = out.len();
        ClassListing { total, shown, classes: out }
    }

    /// Full smali source for one class.
    pub fn smali(&self, class_ref: &str) -> Result<SmaliBody> {
        let class = self
            .resolve_class(class_ref)
            .with_context(|| format!("no class matches {class_ref:?}"))?;
        Ok(SmaliBody {
            class: class.name.as_jni_type(),
            smali: class.to_smali(),
        })
    }

    /// Methods declared by a class.
    pub fn methods(&self, class_ref: &str) -> Result<MethodListing> {
        let class = self
            .resolve_class(class_ref)
            .with_context(|| format!("no class matches {class_ref:?}"))?;
        let methods = class
            .methods
            .iter()
            .map(|m| MethodInfo {
                name: m.name.clone(),
                descriptor: m.signature.to_jni(),
                constructor: m.constructor,
                modifiers: m.modifiers.iter().map(|x| format!("{x:?}")).collect(),
                op_count: m.ops.len(),
            })
            .collect();
        Ok(MethodListing {
            class: class.name.as_jni_type(),
            methods,
        })
    }

    /// Fields declared by a class.
    pub fn fields(&self, class_ref: &str) -> Result<FieldListing> {
        let class = self
            .resolve_class(class_ref)
            .with_context(|| format!("no class matches {class_ref:?}"))?;
        let fields = class
            .fields
            .iter()
            .map(|f| FieldInfo {
                name: f.name.clone(),
                type_jni: f.signature.to_jni(),
                modifiers: f.modifiers.iter().map(|x| format!("{x:?}")).collect(),
            })
            .collect();
        Ok(FieldListing {
            class: class.name.as_jni_type(),
            fields,
        })
    }

    /// Every `invoke-*` call site inside a method. `method_ref` is
    /// `name` (matches first method with that name) or
    /// `name(descriptor)` for unambiguous lookup.
    pub fn method_calls(
        &self,
        class_ref: &str,
        method_ref: &str,
    ) -> Result<MethodCallsResult> {
        let class = self
            .resolve_class(class_ref)
            .with_context(|| format!("no class matches {class_ref:?}"))?;
        let method = resolve_method(class, method_ref)
            .with_context(|| format!("no method matches {method_ref:?}"))?;
        let mut calls = Vec::new();
        for op in &method.ops {
            if let SmaliOp::Op(dex) = op {
                if let Some(site) = invoke_call(dex) {
                    calls.push(site);
                }
            }
        }
        Ok(MethodCallsResult {
            class: class.name.as_jni_type(),
            method: method.name.clone(),
            descriptor: method.signature.to_jni(),
            calls,
        })
    }

    fn resolve_class(&self, needle: &str) -> Option<&SmaliClass> {
        self.dex_classes.iter().find(|c| {
            let jni = c.name.as_jni_type();
            let java = c.name.as_java_type();
            jni == needle || java == needle
        })
    }
}

fn class_info(c: &SmaliClass) -> ClassInfo {
    ClassInfo {
        jni: c.name.as_jni_type(),
        java: c.name.as_java_type(),
        super_class: c.super_class.as_jni_type(),
        field_count: c.fields.len(),
        method_count: c.methods.len(),
    }
}

fn resolve_method<'a>(
    class: &'a SmaliClass,
    needle: &str,
) -> Option<&'a SmaliMethod> {
    // `name(descriptor)` form?
    if let Some((name, rest)) = needle.split_once('(') {
        let desc = format!("({rest}");
        return class
            .methods
            .iter()
            .find(|m| m.name == name && m.signature.to_jni() == desc);
    }
    class.methods.iter().find(|m| m.name == needle)
}

fn invoke_call(op: &DexOp) -> Option<MethodCallSite> {
    let (kind, m): (&str, &MethodRef) = match op {
        DexOp::InvokeVirtual { method, .. } => ("invoke-virtual", method),
        DexOp::InvokeSuper { method, .. } => ("invoke-super", method),
        DexOp::InvokeDirect { method, .. } => ("invoke-direct", method),
        DexOp::InvokeStatic { method, .. } => ("invoke-static", method),
        DexOp::InvokeInterface { method, .. } => ("invoke-interface", method),
        DexOp::InvokeVirtualRange { method, .. } => ("invoke-virtual/range", method),
        DexOp::InvokeSuperRange { method, .. } => ("invoke-super/range", method),
        DexOp::InvokeDirectRange { method, .. } => ("invoke-direct/range", method),
        DexOp::InvokeStaticRange { method, .. } => ("invoke-static/range", method),
        DexOp::InvokeInterfaceRange { method, .. } => ("invoke-interface/range", method),
        _ => return None,
    };
    Some(MethodCallSite {
        kind: kind.to_string(),
        target_class: m.class.clone(),
        target_method: m.name.clone(),
        target_descriptor: m.descriptor.clone(),
    })
}
