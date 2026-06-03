//! In-memory registry of staged AndroidManifest.xml edits.
//!
//! Mirrors `plist_edits::PlistEditRegistry`: each entry is a
//! whole-file replacement of one manifest artifact. Keyed by
//! the artifact's `ArtifactId` so a multi-APK bundle (split
//! APKs in an AAB, say) could carry independent edits per
//! manifest in a later phase.
//!
//! Stored shape: serialised **binary AXML** bytes — the on-disk
//! format AAPT2 produces and Android's resource parser expects.
//! The export flow splices these bytes back into the APK as a
//! drop-in replacement for the original `AndroidManifest.xml`
//! archive entry, then re-signs.
//!
//! In-memory only — closing the bundle drops every staged edit.

use smali::android::binary_xml::AndroidManifest;
use std::collections::HashMap;

/// One staged manifest edit. Holds both the user-facing XML text
/// (what the editor renders) and the serialised AXML bytes (what
/// the export injects). They're kept in sync at commit time.
#[derive(Debug, Clone)]
pub struct ManifestEdit {
    pub artifact: glass_db::ArtifactId,
    /// XML text the user committed last. Same string the editor
    /// re-opens with.
    pub text_xml: String,
    /// Serialised binary AXML bytes. Drop-in replacement for the
    /// source APK's `AndroidManifest.xml` entry.
    pub bytes: Vec<u8>,
}

#[derive(Default, Debug, Clone)]
pub struct ManifestEditRegistry {
    by_artifact: HashMap<glass_db::ArtifactId, ManifestEdit>,
}

impl ManifestEditRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_artifact.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_artifact.len()
    }

    pub fn get(&self, artifact: &glass_db::ArtifactId) -> Option<&ManifestEdit> {
        self.by_artifact.get(artifact)
    }

    pub fn insert(&mut self, edit: ManifestEdit) {
        self.by_artifact.insert(edit.artifact.clone(), edit);
    }

    pub fn remove(&mut self, artifact: &glass_db::ArtifactId) -> Option<ManifestEdit> {
        self.by_artifact.remove(artifact)
    }

    pub fn clear(&mut self) {
        self.by_artifact.clear();
    }

    pub fn entries(&self) -> Vec<&ManifestEdit> {
        let mut out: Vec<&ManifestEdit> = self.by_artifact.values().collect();
        out.sort_by(|a, b| a.artifact.to_string().cmp(&b.artifact.to_string()));
        out
    }
}

/// Decode binary AXML bytes to user-readable XML text via
/// smali's AndroidManifest DOM. Returns a textual XML string
/// that the editor can render and round-trip back to bytes.
///
/// Smali's `to_string` writes flat — every element on one
/// line — which is fine for diff round-tripping but unreadable
/// in a code editor. We re-emit through quick-xml's indenting
/// writer so the manifest looks like what `apktool d` would
/// produce. 4-space indent matches AOSP convention.
pub fn load_as_xml(bytes: &[u8]) -> Result<String, String> {
    let manifest = AndroidManifest::from_bytes(bytes)
        .map_err(|e| format!("parsing binary manifest: {e}"))?;
    let flat = manifest
        .to_string()
        .map_err(|e| format!("rendering manifest as XML: {e}"))?;
    pretty_print_xml(&flat)
}

/// Re-emit a flat XML string with line breaks + 4-space
/// indentation by walking it through quick-xml's indenting
/// writer. Falls back to the original on parse failure — better
/// to show ugly XML than nothing.
fn pretty_print_xml(flat: &str) -> Result<String, String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    use quick_xml::writer::Writer;

    let mut reader = Reader::from_str(flat);
    reader.config_mut().trim_text(true);
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 4);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(ev) => writer
                .write_event(ev)
                .map_err(|e| format!("rewriting manifest XML: {e}"))?,
            Err(e) => return Err(format!("re-parsing manifest XML: {e}")),
        }
        buf.clear();
    }
    String::from_utf8(writer.into_inner())
        .map_err(|e| format!("manifest XML wasn't UTF-8: {e}"))
}

/// Validate XML text by attempting to parse it as a manifest.
/// Returns Ok(()) on success or a human-readable error string
/// suitable for the editor's `save_error` slot.
pub fn validate_xml(text: &str) -> Result<(), String> {
    AndroidManifest::from_string(text)
        .map(|_| ())
        .map_err(|e| format!("manifest parse error: {e}"))
}

/// Serialise XML text back to binary AXML. The text must already
/// pass `validate_xml`; this is the commit-time hot path that
/// produces the bytes the export flow will splice in.
pub fn serialise_to_bytes(text: &str) -> Result<Vec<u8>, String> {
    let manifest = AndroidManifest::from_string(text)
        .map_err(|e| format!("parsing manifest: {e}"))?;
    manifest
        .to_bytes()
        .map_err(|e| format!("serialising binary manifest: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal but realistic manifest covering namespace
    /// declarations, the `android:` prefix on attributes, and a
    /// nested element with text content — the things our
    /// round-trip is most likely to drop.
    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="com.example.app">
    <uses-permission android:name="android.permission.INTERNET"/>
    <application android:label="Example">
        <activity android:name=".MainActivity"/>
    </application>
</manifest>"#;

    #[test]
    fn validate_accepts_sample() {
        validate_xml(SAMPLE_XML).expect("sample parses");
    }

    #[test]
    fn validate_rejects_malformed() {
        let bad = "<manifest><application></manifest>";
        assert!(validate_xml(bad).is_err());
    }

    #[test]
    fn round_trip_preserves_package() {
        let bytes = serialise_to_bytes(SAMPLE_XML).expect("encode");
        let text2 = load_as_xml(&bytes).expect("decode");
        // Re-encoding loses literal formatting but the package
        // attribute survives.
        assert!(text2.contains("com.example.app"));
        assert!(text2.contains("android.permission.INTERNET"));
        assert!(text2.contains(".MainActivity"));
    }

    /// Smali's `to_string` emits flat XML on a single line —
    /// `load_as_xml` must re-indent so the editor renders
    /// something readable rather than a 5kB single-row blob.
    #[test]
    fn load_as_xml_indents_output() {
        let bytes = serialise_to_bytes(SAMPLE_XML).expect("encode");
        let text = load_as_xml(&bytes).expect("decode");
        // More than one line means indentation kicked in.
        assert!(
            text.lines().count() > 3,
            "expected multi-line output, got {} lines: {text}",
            text.lines().count()
        );
        // 4-space indent on first nested element. The
        // <uses-permission> child should be indented.
        assert!(
            text.contains("\n    <uses-permission"),
            "expected 4-space indent before <uses-permission>; got: {text}"
        );
    }
}
