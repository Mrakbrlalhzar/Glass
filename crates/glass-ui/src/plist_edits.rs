//! In-memory registry of staged Info.plist edits.
//!
//! Mirrors `smali_edits::SmaliEditRegistry`: each entry is a
//! whole-file replacement of one plist artifact (Info.plist or
//! a framework's *.plist). Keyed by the artifact's `ArtifactId`
//! so a bundle with multiple plists can carry independent
//! edits for each.
//!
//! Stored shape: serialised bytes in the **original on-disk
//! format** (binary `bplist00` or XML), so the IPA export
//! flow can substitute the file verbatim without having to
//! re-detect format.
//!
//! In-memory only — closing the bundle drops every staged
//! edit. The plumbing into `export-patched` lands in a
//! follow-up commit alongside the IPA export path's plist
//! override map.
//!
//! The runtime model is "the source-of-truth is the edited
//! bytes". On reopen we re-parse the bytes back into a
//! `plist::Value` if needed for the structured viewer.

use std::collections::HashMap;

/// What on-disk encoding the plist used originally — preserved
/// so the export can round-trip the file in its original form.
/// Apps overwhelmingly ship binary plists; preserving format
/// reduces blast radius for downstream tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlistFormat {
    Binary,
    Xml,
}

/// One staged plist edit. Holds both the user-facing XML text
/// (what the editor renders) and the serialised bytes (what
/// the export injects). They're kept in sync at commit time.
#[derive(Debug, Clone)]
pub struct PlistEdit {
    pub artifact: glass_db::ArtifactId,
    pub source_format: PlistFormat,
    /// XML text the user committed last. Same string the editor
    /// re-opens with.
    pub text_xml: String,
    /// Serialised bytes in `source_format`. Drop-in replacement
    /// for the source-IPA's archive entry.
    pub bytes: Vec<u8>,
}

#[derive(Default, Debug, Clone)]
pub struct PlistEditRegistry {
    by_artifact: HashMap<glass_db::ArtifactId, PlistEdit>,
}

impl PlistEditRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_artifact.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_artifact.len()
    }

    pub fn get(&self, artifact: &glass_db::ArtifactId) -> Option<&PlistEdit> {
        self.by_artifact.get(artifact)
    }

    pub fn insert(&mut self, edit: PlistEdit) {
        self.by_artifact.insert(edit.artifact.clone(), edit);
    }

    pub fn remove(&mut self, artifact: &glass_db::ArtifactId) -> Option<PlistEdit> {
        self.by_artifact.remove(artifact)
    }

    pub fn clear(&mut self) {
        self.by_artifact.clear();
    }

    pub fn entries(&self) -> Vec<&PlistEdit> {
        let mut out: Vec<&PlistEdit> = self.by_artifact.values().collect();
        // Stable order by artifact id string — same convention
        // as the other registries.
        out.sort_by(|a, b| a.artifact.to_string().cmp(&b.artifact.to_string()));
        out
    }
}

/// Detect the on-disk format of a plist by its magic. Binary
/// plists start with the literal `bplist` string; everything
/// else is treated as XML (which covers both proper UTF-8 XML
/// and the rare "openstep" / pre-XML variants — `plist` crate
/// auto-detects on parse).
pub fn detect_format(bytes: &[u8]) -> PlistFormat {
    if bytes.len() >= 6 && &bytes[..6] == b"bplist" {
        PlistFormat::Binary
    } else {
        PlistFormat::Xml
    }
}

/// Parse the raw bytes (binary or XML) into a `plist::Value`
/// and serialise as XML for the editor. Returns the XML text
/// plus the detected source format so the editor can write
/// back in the original encoding.
pub fn load_as_xml(bytes: &[u8]) -> Result<(String, PlistFormat), String> {
    let format = detect_format(bytes);
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes))
        .map_err(|e| format!("parsing plist: {e}"))?;
    let mut buf: Vec<u8> = Vec::with_capacity(bytes.len().max(256));
    plist::to_writer_xml(&mut buf, &value)
        .map_err(|e| format!("serialising plist to XML: {e}"))?;
    let text =
        String::from_utf8(buf).map_err(|e| format!("plist XML wasn't UTF-8: {e}"))?;
    Ok((text, format))
}

/// Validate XML text by attempting to parse it as a plist.
/// Returns Ok(()) on success or a human-readable error string
/// suitable for the editor's `save_error` slot.
pub fn validate_xml(text: &str) -> Result<(), String> {
    plist::Value::from_reader_xml(std::io::Cursor::new(text.as_bytes()))
        .map(|_| ())
        .map_err(|e| format!("plist parse error: {e}"))
}

/// Serialise XML text back to the requested on-disk format.
/// The text must already pass `validate_xml`; this is the
/// commit-time hot path that produces the bytes the export
/// flow will splice in.
pub fn serialise_to_bytes(
    text: &str,
    format: PlistFormat,
) -> Result<Vec<u8>, String> {
    let value = plist::Value::from_reader_xml(std::io::Cursor::new(text.as_bytes()))
        .map_err(|e| format!("parsing plist: {e}"))?;
    let mut buf: Vec<u8> = Vec::with_capacity(text.len());
    match format {
        PlistFormat::Binary => plist::to_writer_binary(&mut buf, &value)
            .map_err(|e| format!("serialising binary plist: {e}"))?,
        PlistFormat::Xml => plist::to_writer_xml(&mut buf, &value)
            .map_err(|e| format!("serialising XML plist: {e}"))?,
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>com.example.app</string>
    <key>CFBundleVersion</key>
    <string>1.0</string>
</dict>
</plist>"#;

    #[test]
    fn detect_xml_format() {
        assert_eq!(detect_format(SAMPLE_XML.as_bytes()), PlistFormat::Xml);
    }

    #[test]
    fn detect_binary_format() {
        assert_eq!(detect_format(b"bplist00..."), PlistFormat::Binary);
    }

    #[test]
    fn xml_round_trip_through_value() {
        let (text, format) = load_as_xml(SAMPLE_XML.as_bytes()).expect("load");
        assert_eq!(format, PlistFormat::Xml);
        assert!(text.contains("CFBundleIdentifier"));
        validate_xml(&text).expect("validate");
        let bytes = serialise_to_bytes(&text, PlistFormat::Xml).expect("write");
        // Parsing the written bytes back should yield the same
        // root key set.
        let v = plist::Value::from_reader(std::io::Cursor::new(&bytes))
            .expect("reparse");
        let dict = v.as_dictionary().expect("dict");
        assert!(dict.contains_key("CFBundleIdentifier"));
    }

    #[test]
    fn binary_round_trip_preserves_format() {
        // Make binary input from an XML round-trip.
        let (text, _) = load_as_xml(SAMPLE_XML.as_bytes()).expect("load");
        let binary = serialise_to_bytes(&text, PlistFormat::Binary).expect("write bin");
        assert_eq!(detect_format(&binary), PlistFormat::Binary);
        // Re-load the binary; should parse as binary still.
        let (text2, format2) = load_as_xml(&binary).expect("reload");
        assert_eq!(format2, PlistFormat::Binary);
        assert!(text2.contains("CFBundleIdentifier"));
    }

    #[test]
    fn validate_catches_malformed() {
        let result = validate_xml("<plist><dict><key>unclosed</plist>");
        assert!(result.is_err());
    }
}
