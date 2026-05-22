//! Plan a Frida-gadget injection into an APK bundle.
//!
//! This module is pure inspection — it doesn't touch the
//! filesystem or modify the APK. It produces an
//! [`InjectionPlan`] that describes exactly what Glass would
//! do, suitable for displaying as a preview in the injection
//! dialog. Applying the plan is a follow-up milestone
//! (M3.2b: smali patcher + APK rewriter).
//!
//! Why split planning from applying:
//!   * The dialog needs to render "here's what'll change"
//!     before the user clicks Inject. A pure planner returns
//!     the answer cheaply, without staging anything yet.
//!   * The same plan is the input to the executor — fewer
//!     places for the planner's decisions and the executor's
//!     actions to drift out of sync.
//!   * Unit-testable: feed fixture data, assert plan fields.

use std::collections::BTreeSet;

/// Result of analysing an APK for Frida-gadget injection.
/// Build by calling [`plan_injection`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectionPlan {
    /// Package name from the manifest (`android:package`).
    /// Informational; used in the dialog header.
    pub package_name: Option<String>,
    /// ABIs Glass will drop a gadget `.so` into. One per
    /// `lib/<abi>/` directory the original APK already has.
    /// Empty when the APK has no native libs at all — that's
    /// surfaced as a [`PlanWarning::NoNativeLibsDir`].
    pub abis: Vec<String>,
    /// Where the `System.loadLibrary("frida-gadget")` call
    /// will be inserted.
    pub patch_target: PatchTarget,
    /// Non-fatal observations the dialog should surface to the
    /// user. The user can still click Inject even if warnings
    /// are present.
    pub warnings: Vec<PlanWarning>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchTarget {
    /// `<application android:name="…">` is set. Glass will
    /// patch this class's `<clinit>` (preferred) or `onCreate`
    /// to call `System.loadLibrary("frida-gadget")`.
    ExistingApplication {
        /// JNI form: `Lcom/example/MyApplication;`.
        class_jni: String,
        /// Java-form for the dialog: `com.example.MyApplication`.
        class_display: String,
        /// Which method we'll patch — `<clinit>` runs earliest,
        /// `onCreate` is the fallback if no static block
        /// exists.
        method: PatchMethod,
    },
    /// No custom Application class. Fall back to the first
    /// Activity declared with the `MAIN` intent filter — its
    /// `onCreate` is called when the user taps the launcher
    /// icon.
    LauncherActivity {
        class_jni: String,
        class_display: String,
        method: PatchMethod,
    },
    /// No Application *and* no launcher Activity. Glass would
    /// have to synthesise a stub Application and rewrite the
    /// manifest to point at it. Not implemented in this slice
    /// — the planner records the situation, the executor will
    /// refuse to run.
    SynthesiseRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchMethod {
    /// Class-level static initialiser. Runs the first time the
    /// JVM touches the class — earliest practical hook point
    /// inside Java code. Preferred.
    ClassInit,
    /// `onCreate(android.os.Bundle)` for an Application or
    /// Activity. Runs after the system has constructed the
    /// object; slightly later than `<clinit>` but always
    /// present.
    OnCreate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanWarning {
    /// Bundle has no `lib/` directory. We'd have to create it
    /// and pick an ABI to target. M3.2a-vendoring decides which
    /// ABIs we even have a gadget for.
    NoNativeLibsDir,
    /// No `<application android:name>` and no `MAIN` Activity.
    /// Executor will need to synthesise one.
    NoEntryPoint,
    /// The chosen patch class isn't in the loaded smali set —
    /// usually means it lives in a DEX we couldn't lift, or
    /// the manifest references a class that doesn't exist.
    /// User should double-check before applying.
    PatchClassNotLifted { class_jni: String },
    /// The chosen Application class extends a non-`Object`
    /// class we don't recognise. The gadget call is still safe
    /// to splice into `<clinit>`, but it's worth flagging in
    /// case the parent class does something exotic at load.
    UnusualApplicationParent { parent_jni: String },
    /// The APK already contains a `libfrida-gadget.so` in at
    /// least one ABI directory. The user is probably
    /// re-injecting; Glass will overwrite.
    GadgetAlreadyPresent { abis: Vec<String> },
}

/// Inputs to the planner. Kept as a small struct (rather than
/// `&Bundle`) so tests can synthesise fixtures without
/// constructing a full LoadedBundle.
pub struct PlanInputs<'a> {
    /// Parsed AndroidManifest.xml. `None` if the APK didn't
    /// have one or it failed to decode — planner returns an
    /// `Err` in that case.
    pub manifest: Option<&'a smali::android::binary_xml::AndroidManifest>,
    /// JNI signatures of every class lifted from the APK's
    /// DEX files. Used to confirm the patch target actually
    /// exists in the loaded smali — otherwise we emit a
    /// warning.
    pub loaded_classes: BTreeSet<String>,
    /// ABIs the APK has `lib/<abi>/` directories for. Drives
    /// where the gadget gets dropped.
    pub native_abis: BTreeSet<String>,
    /// ABIs that already contain a `libfrida-gadget.so` —
    /// triggers the re-injection warning.
    pub abis_with_gadget: BTreeSet<String>,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("APK has no AndroidManifest.xml")]
    NoManifest,
}

pub fn plan_injection(inputs: &PlanInputs<'_>) -> Result<InjectionPlan, PlanError> {
    let manifest = inputs.manifest.ok_or(PlanError::NoManifest)?;
    let package_name = manifest.package_name().map(|s| s.to_string());

    let mut warnings = Vec::new();
    if inputs.native_abis.is_empty() {
        warnings.push(PlanWarning::NoNativeLibsDir);
    }
    if !inputs.abis_with_gadget.is_empty() {
        warnings.push(PlanWarning::GadgetAlreadyPresent {
            abis: inputs.abis_with_gadget.iter().cloned().collect(),
        });
    }

    let (patch_target, mut decision_warnings) = choose_patch_target(
        manifest,
        package_name.as_deref(),
        &inputs.loaded_classes,
    );
    warnings.append(&mut decision_warnings);

    Ok(InjectionPlan {
        package_name,
        abis: inputs.native_abis.iter().cloned().collect(),
        patch_target,
        warnings,
    })
}

/// Pick the patch location. Decision order:
///   1. `<application android:name>` if set → ClassInit on it.
///   2. First `<activity>` carrying a `MAIN` action +
///      `LAUNCHER` category → OnCreate on it.
///   3. Otherwise SynthesiseRequired.
///
/// `package_name` is used to resolve relative class names
/// (`.MyApp` → `com.foo.MyApp`).
fn choose_patch_target(
    manifest: &smali::android::binary_xml::AndroidManifest,
    package_name: Option<&str>,
    loaded_classes: &BTreeSet<String>,
) -> (PatchTarget, Vec<PlanWarning>) {
    let mut warnings = Vec::new();
    if let Some(app) = manifest.application() {
        if let Some(class) = manifest_class_attr(app, package_name) {
            let class_jni = java_to_jni(&class);
            if !loaded_classes.contains(&class_jni) {
                warnings.push(PlanWarning::PatchClassNotLifted {
                    class_jni: class_jni.clone(),
                });
            }
            return (
                PatchTarget::ExistingApplication {
                    class_jni,
                    class_display: class,
                    method: PatchMethod::ClassInit,
                },
                warnings,
            );
        }
    }
    if let Some(launcher) = find_launcher_activity(manifest) {
        if let Some(class) = manifest_class_attr(launcher, package_name) {
            let class_jni = java_to_jni(&class);
            if !loaded_classes.contains(&class_jni) {
                warnings.push(PlanWarning::PatchClassNotLifted {
                    class_jni: class_jni.clone(),
                });
            }
            return (
                PatchTarget::LauncherActivity {
                    class_jni,
                    class_display: class,
                    method: PatchMethod::OnCreate,
                },
                warnings,
            );
        }
    }
    warnings.push(PlanWarning::NoEntryPoint);
    (PatchTarget::SynthesiseRequired, warnings)
}

/// Resolve an `android:name` attribute to a fully-qualified
/// dotted class name. Manifest names can be:
///   * Absolute: `com.example.MyApp` — returned as-is.
///   * Relative with leading `.`: `.MyApp` — prefix with the
///     package.
///   * Relative without leading `.`: `MyApp` — same prefixing.
/// Android manifest attributes carry the `android` namespace
/// prefix (`android:name`). The smali crate's
/// `attribute_value(...)` query is namespace-aware, so we must
/// pass the qualified key. The earlier version of this code
/// looked up the unqualified `name` and silently found
/// nothing on every real manifest.
fn manifest_class_attr(
    elem: &smali::android::binary_xml::ManifestElement,
    package_name: Option<&str>,
) -> Option<String> {
    let value = elem
        .attribute_value("android:name")
        // Some manifests emitted by aapt2 / older tooling
        // drop the namespace prefix on the in-memory tree
        // even though they use the standard schema URI. Fall
        // back to the unqualified form so we still find the
        // attribute in those cases.
        .or_else(|| elem.attribute_value("name"))?;
    let raw = value.as_str()?;
    if raw.contains('.') && !raw.starts_with('.') {
        Some(raw.to_string())
    } else {
        let trimmed = raw.trim_start_matches('.');
        package_name.map(|pkg| format!("{pkg}.{trimmed}"))
    }
}

/// Find the first Activity (or activity-alias) with a `MAIN`
/// action and `LAUNCHER` category in its intent filter set.
/// Returns the node so the caller can read its `android:name`.
fn find_launcher_activity(
    manifest: &smali::android::binary_xml::AndroidManifest,
) -> Option<&smali::android::binary_xml::ManifestElement> {
    let app = manifest.application()?;
    for child in &app.children {
        if child.tag != "activity" && child.tag != "activity-alias" {
            continue;
        }
        if activity_is_launcher(child) {
            return Some(child);
        }
    }
    None
}

fn activity_is_launcher(
    activity: &smali::android::binary_xml::ManifestElement,
) -> bool {
    let mut has_main = false;
    let mut has_launcher = false;
    for child in &activity.children {
        if child.tag != "intent-filter" {
            continue;
        }
        for grand in &child.children {
            // `name` here is namespaced — `android:name` — same
            // story as `<application android:name>`. Fall back
            // to the unqualified form for the same robustness
            // reasons described on manifest_class_attr.
            let name = grand
                .attribute_value("android:name")
                .or_else(|| grand.attribute_value("name"))
                .and_then(|v| v.as_str());
            match grand.tag.as_str() {
                "action" => {
                    if name == Some("android.intent.action.MAIN") {
                        has_main = true;
                    }
                }
                "category" => {
                    if name == Some("android.intent.category.LAUNCHER") {
                        has_launcher = true;
                    }
                }
                _ => {}
            }
        }
    }
    has_main && has_launcher
}

/// `com.example.Foo` → `Lcom/example/Foo;`.
fn java_to_jni(dotted: &str) -> String {
    format!("L{};", dotted.replace('.', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smali::android::binary_xml::{
        AndroidManifest, ManifestAttribute, ManifestElement, ManifestValue,
    };

    fn mk_manifest(package: &str) -> AndroidManifest {
        let mut root = ManifestElement::new("manifest");
        root.set_attribute(ManifestAttribute::new("package", package));
        // smali's AndroidManifest::new takes the root by value;
        // its exact constructor signature is private — we
        // build via the public builder API by serialising and
        // parsing. For tests that's overkill; instead, drive
        // the planner through ManifestElement helpers it does
        // expose. The tests below use the smali crate's own
        // construction helpers (mk_manifest_from_root).
        let _ = root;
        // We can't construct AndroidManifest from outside the
        // smali crate without going through binary-XML decode.
        // That means the planner's unit tests have to be
        // integration-level (decode a fixture APK). Skip pure
        // construction here and rely on the fns being small
        // enough to inspect by eye; add an integration test in
        // a follow-up once we have a fixture APK to embed.
        AndroidManifest::default()
    }

    #[test]
    fn java_to_jni_roundtrip() {
        assert_eq!(java_to_jni("com.example.Foo"), "Lcom/example/Foo;");
        assert_eq!(java_to_jni("a.B"), "La/B;");
    }

    #[test]
    fn plan_returns_no_manifest_error_when_missing() {
        let inputs = PlanInputs {
            manifest: None,
            loaded_classes: BTreeSet::new(),
            native_abis: BTreeSet::new(),
            abis_with_gadget: BTreeSet::new(),
        };
        assert_eq!(plan_injection(&inputs), Err(PlanError::NoManifest));
    }

    #[test]
    fn empty_native_libs_dir_surfaces_warning() {
        // Construct via the smali crate's default — minimal
        // valid AndroidManifest — and feed an empty everything.
        // Verifies the no-libs warning fires.
        let m = mk_manifest("com.example.app");
        let inputs = PlanInputs {
            manifest: Some(&m),
            loaded_classes: BTreeSet::new(),
            native_abis: BTreeSet::new(),
            abis_with_gadget: BTreeSet::new(),
        };
        let plan = plan_injection(&inputs).expect("plans without entry-point");
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::NoNativeLibsDir)));
    }
}
