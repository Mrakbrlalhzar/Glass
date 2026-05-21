//! Apply an [`InjectionPlan`] to a parsed `SmaliClass`.
//!
//! Inserts the two-op sequence
//!
//! ```text
//! const-string vN, "frida-gadget"
//! invoke-static {vN}, Ljava/lang/System;->loadLibrary(Ljava/lang/String;)V
//! ```
//!
//! at the head of the chosen method, allocating a fresh local
//! register (`vN` where N == the method's previous `locals`
//! count) so the rest of the method's register usage shifts
//! by exactly one. The method's `locals` field is bumped by
//! one to match.
//!
//! For Application classes that don't have a `<clinit>` yet,
//! a fresh one is synthesised:
//!
//! ```text
//! .method static constructor <clinit>()V
//!     .locals 1
//!     const-string v0, "frida-gadget"
//!     invoke-static {v0}, Ljava/lang/System;->loadLibrary(Ljava/lang/String;)V
//!     return-void
//! .end method
//! ```

use smali::smali_ops::{DexOp, MethodRef, SmaliRegister};
use smali::types::{
    MethodSignature, Modifier, SmaliClass, SmaliMethod, SmaliOp,
};

use crate::injection::{InjectionPlan, PatchMethod, PatchTarget};

/// The library name that `System.loadLibrary(...)` receives.
/// Matches the gadget binary's expected filename
/// (`libfrida-gadget.so` → loadLibrary("frida-gadget"); the
/// linker prepends `lib` and appends `.so`).
pub const GADGET_LIBRARY_NAME: &str = "frida-gadget";

#[derive(Debug, Clone, thiserror::Error)]
pub enum PatchError {
    #[error("plan target {0} not implemented yet (synthesising an Application class needs M3.2d)")]
    SynthesiseNotImplemented(&'static str),
    #[error("class {class_jni} doesn't contain a method {method_name} matching the planner's choice")]
    MethodNotFound { class_jni: String, method_name: String },
}

/// Apply the plan to `class` and return a mutated clone. Caller
/// then stages the result through the normal smali-edits path.
pub fn apply_plan(
    class: &SmaliClass,
    plan: &InjectionPlan,
) -> Result<SmaliClass, PatchError> {
    let (method_name, method_kind) = match &plan.patch_target {
        PatchTarget::ExistingApplication { method, .. } => {
            (method_name_for(*method), *method)
        }
        PatchTarget::LauncherActivity { method, .. } => {
            (method_name_for(*method), *method)
        }
        PatchTarget::SynthesiseRequired => {
            return Err(PatchError::SynthesiseNotImplemented(
                "Synthesise required",
            ));
        }
    };
    let mut out = class.clone();
    let class_jni = out.name.as_jni_type();
    // Find the target method by name. For Application classes
    // the method is either `<clinit>` (no args, void return) or
    // `onCreate()V`; for Activities it's `onCreate(Bundle)V`.
    // We match by name only — the planner already picked the
    // method's signature implicitly by its kind.
    let method_idx = out.methods.iter().position(|m| m.name == method_name);
    match method_idx {
        Some(idx) => {
            patch_existing(&mut out.methods[idx]);
        }
        None => {
            // The planner chose <clinit> on an existing class
            // that didn't declare one. Synthesise it.
            if matches!(method_kind, PatchMethod::ClassInit) {
                out.methods.push(synthesise_clinit());
            } else {
                return Err(PatchError::MethodNotFound {
                    class_jni,
                    method_name: method_name.to_string(),
                });
            }
        }
    }
    Ok(out)
}

fn method_name_for(m: PatchMethod) -> &'static str {
    match m {
        PatchMethod::ClassInit => "<clinit>",
        PatchMethod::OnCreate => "onCreate",
    }
}

/// Patch an existing method by allocating a fresh local
/// register (`v<locals>`) and prepending the load sequence.
/// Existing register references aren't touched — we only
/// claim a new register at the top of the local range.
fn patch_existing(method: &mut SmaliMethod) {
    let new_reg_index = method.locals as u16;
    let reg = SmaliRegister::Local(new_reg_index);
    let preamble = vec![
        SmaliOp::Op(DexOp::ConstString {
            dest: reg.clone(),
            value: GADGET_LIBRARY_NAME.to_string(),
        }),
        SmaliOp::Op(DexOp::InvokeStatic {
            registers: vec![reg],
            method: load_library_methodref(),
        }),
    ];
    // Splice at the very top of the body. Any `.locals` /
    // `.line` / `.registers` directives are handled by the
    // writer based on `method.locals` + `method.registers`, so
    // we don't have to be careful about preserving them at the
    // op-vec level.
    let original_ops = std::mem::take(&mut method.ops);
    method.ops = preamble;
    method.ops.extend(original_ops);
    // Bump locals by 1 to cover our new register. If the
    // method had an explicit `.registers N` directive we also
    // bump that — the writer prefers `.registers` when set.
    method.locals = method.locals.saturating_add(1);
    if let Some(r) = method.registers.as_mut() {
        *r = r.saturating_add(1);
    }
}

fn synthesise_clinit() -> SmaliMethod {
    let reg = SmaliRegister::Local(0);
    SmaliMethod {
        name: "<clinit>".to_string(),
        modifiers: vec![Modifier::Static],
        constructor: true,
        signature: MethodSignature::from_jni("()V"),
        locals: 1,
        registers: None,
        params: Vec::new(),
        annotations: Vec::new(),
        ops: vec![
            SmaliOp::Op(DexOp::ConstString {
                dest: reg.clone(),
                value: GADGET_LIBRARY_NAME.to_string(),
            }),
            SmaliOp::Op(DexOp::InvokeStatic {
                registers: vec![reg],
                method: load_library_methodref(),
            }),
            SmaliOp::Op(DexOp::ReturnVoid),
        ],
    }
}

fn load_library_methodref() -> MethodRef {
    MethodRef {
        class: "Ljava/lang/System;".to_string(),
        name: "loadLibrary".to_string(),
        descriptor: "(Ljava/lang/String;)V".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smali::types::ObjectIdentifier;

    fn empty_class(jni: &str) -> SmaliClass {
        SmaliClass {
            name: ObjectIdentifier::from_jni_type(jni),
            modifiers: vec![Modifier::Public],
            source: None,
            super_class: ObjectIdentifier::from_jni_type(
                "Landroid/app/Application;",
            ),
            implements: vec![],
            annotations: vec![],
            fields: vec![],
            methods: vec![],
            file_path: None,
        }
    }

    fn plan_for_clinit() -> InjectionPlan {
        InjectionPlan {
            package_name: Some("com.example".into()),
            abis: vec!["arm64-v8a".into()],
            patch_target: PatchTarget::ExistingApplication {
                class_jni: "Lcom/example/App;".into(),
                class_display: "com.example.App".into(),
                method: PatchMethod::ClassInit,
            },
            warnings: vec![],
        }
    }

    #[test]
    fn synthesises_clinit_when_missing() {
        let class = empty_class("Lcom/example/App;");
        let patched = apply_plan(&class, &plan_for_clinit()).unwrap();
        assert_eq!(patched.methods.len(), 1);
        let m = &patched.methods[0];
        assert_eq!(m.name, "<clinit>");
        assert!(m.constructor);
        assert_eq!(m.locals, 1);
        assert_eq!(m.ops.len(), 3);
    }

    #[test]
    fn prepends_to_existing_clinit() {
        let mut class = empty_class("Lcom/example/App;");
        class.methods.push(SmaliMethod {
            name: "<clinit>".into(),
            modifiers: vec![Modifier::Static],
            constructor: true,
            signature: MethodSignature::from_jni("()V"),
            locals: 0,
            registers: None,
            params: vec![],
            annotations: vec![],
            ops: vec![SmaliOp::Op(DexOp::ReturnVoid)],
        });
        let patched = apply_plan(&class, &plan_for_clinit()).unwrap();
        assert_eq!(patched.methods.len(), 1);
        let m = &patched.methods[0];
        // 2 injected + 1 original (return-void).
        assert_eq!(m.ops.len(), 3);
        assert_eq!(m.locals, 1, "locals should have been bumped");
        // First op should be const-string of "frida-gadget".
        match &m.ops[0] {
            SmaliOp::Op(DexOp::ConstString { value, .. }) => {
                assert_eq!(value, GADGET_LIBRARY_NAME);
            }
            other => panic!("expected const-string, got {other:?}"),
        }
    }

    #[test]
    fn synthesise_required_returns_error() {
        let class = empty_class("Lcom/example/App;");
        let plan = InjectionPlan {
            package_name: Some("com.example".into()),
            abis: vec!["arm64-v8a".into()],
            patch_target: PatchTarget::SynthesiseRequired,
            warnings: vec![],
        };
        assert!(apply_plan(&class, &plan).is_err());
    }
}
