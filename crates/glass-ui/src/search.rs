//! Command-palette search index.
//!
//! Built lazily by the loader on a background task. Flat
//! `Vec<SearchEntry>` scanned linearly per keystroke — fine up to
//! ~200k entries, which is what a big native lib + DEX yields.

use crate::{LeafKind, LoadedBundle, NativeSectionKind};

#[derive(Clone, Debug)]
pub enum SearchJump {
    Listing { artifact: glass_db::ArtifactId, section: String, addr: u64 },
    Hex { artifact: glass_db::ArtifactId, section: String, addr: u64 },
    SmaliClass { class_jni: String },
    SectionMap { artifact: glass_db::ArtifactId },
}

#[derive(Clone, Debug)]
pub struct SearchEntry {
    /// Primary display string we match against.
    pub display: String,
    /// Right-side chip (e.g. ".text · libfoo.so" or "method · com.example.Foo").
    pub chip: String,
    /// Single-character kind glyph for the left column.
    pub kind_glyph: &'static str,
    pub jump: SearchJump,
}

#[derive(Default)]
pub struct SearchIndex {
    pub entries: Vec<SearchEntry>,
}

impl SearchIndex {
    /// Filter the index against a query and return up to `cap` results,
    /// ranked: prefix-match > substring > char-subsequence, then by
    /// display length (shorter = closer).
    pub fn filter(&self, query: &str, cap: usize) -> Vec<&SearchEntry> {
        if query.is_empty() {
            return Vec::new();
        }
        let q = query.to_lowercase();
        let mut scored: Vec<(u8, usize, &SearchEntry)> = Vec::new();
        for e in &self.entries {
            let hay = e.display.to_lowercase();
            let tier = if hay.starts_with(&q) {
                0
            } else if hay.contains(&q) {
                1
            } else if is_subsequence(&q, &hay) {
                2
            } else {
                continue;
            };
            scored.push((tier, e.display.len(), e));
        }
        scored.sort_by_key(|&(tier, len, _)| (tier, len));
        scored.into_iter().take(cap).map(|(_, _, e)| e).collect()
    }
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut h = hay.chars();
    'outer: for nc in needle.chars() {
        for hc in h.by_ref() {
            if hc == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// Build a flat search index of every navigable target in `bundle`:
///   - Native symbols (text + data).
///   - DEX classes, methods, fields.
///   - Printable ASCII strings ≥4 chars from non-text non-bss
///     non-debug, non-zero-base sections.
///   - Section names (so "rodata" jumps to that section).
pub fn build_search_index(bundle: &LoadedBundle) -> SearchIndex {
    let mut entries: Vec<SearchEntry> = Vec::new();
    let mut artifact_label: std::collections::HashMap<glass_db::ArtifactId, String> =
        std::collections::HashMap::new();

    for ((aid, _name), _) in bundle.text_sections.iter() {
        artifact_label
            .entry(aid.clone())
            .or_insert_with(|| short_artifact_label(bundle, aid));
    }

    // Native symbols.
    for (aid, sm) in bundle.symbol_maps.iter() {
        let alabel = artifact_label
            .entry(aid.clone())
            .or_insert_with(|| short_artifact_label(bundle, aid))
            .clone();
        for sym in sm.iter() {
            let section = bundle
                .text_section_for_addr(aid, sym.address)
                .or_else(|| bundle.data_section_for_addr(aid, sym.address))
                .map(|s| s.to_string())
                .unwrap_or_default();
            let jump = if !section.is_empty()
                && bundle.text_sections.contains_key(&(aid.clone(), section.clone()))
            {
                SearchJump::Listing {
                    artifact: aid.clone(),
                    section: section.clone(),
                    addr: sym.address,
                }
            } else if !section.is_empty() {
                SearchJump::Hex {
                    artifact: aid.clone(),
                    section: section.clone(),
                    addr: sym.address,
                }
            } else {
                SearchJump::SectionMap { artifact: aid.clone() }
            };
            entries.push(SearchEntry {
                display: sym.display_name.clone(),
                chip: if section.is_empty() {
                    alabel.clone()
                } else {
                    format!("{section} · {alabel}")
                },
                kind_glyph: "ƒ",
                jump,
            });
        }
    }

    // DEX classes / methods / fields.
    let kinds_iter = bundle.kinds.iter().enumerate();
    for (leaf_id, k) in kinds_iter {
        if let LeafKind::SmaliClass { class_jni } = k {
            let display = jni_to_dotted(class_jni);
            let simple = display.rsplit('.').next().unwrap_or(&display).to_string();
            entries.push(SearchEntry {
                display: display.clone(),
                chip: "class".to_string(),
                kind_glyph: "Ⓒ",
                jump: SearchJump::SmaliClass {
                    class_jni: class_jni.clone(),
                },
            });
            if let Some(body) = bundle.bodies.get(leaf_id) {
                for line in body.lines() {
                    let trimmed = line.trim_start();
                    if let Some(rest) = trimmed.strip_prefix(".method ") {
                        if let Some(name) = rest.split_whitespace().last() {
                            entries.push(SearchEntry {
                                display: format!("{simple}.{name}"),
                                chip: format!("method · {display}"),
                                kind_glyph: "ƒ",
                                jump: SearchJump::SmaliClass {
                                    class_jni: class_jni.clone(),
                                },
                            });
                        }
                    } else if let Some(rest) = trimmed.strip_prefix(".field ") {
                        if let Some(name) = rest.split_whitespace().last() {
                            entries.push(SearchEntry {
                                display: format!("{simple}.{name}"),
                                chip: format!("field · {display}"),
                                kind_glyph: "ᕀ",
                                jump: SearchJump::SmaliClass {
                                    class_jni: class_jni.clone(),
                                },
                            });
                        }
                    }
                }
            }
        }
    }

    // Section names — useful for "rodata" style searches.
    for (aid, sections) in bundle.native_sections.iter() {
        let alabel = artifact_label
            .entry(aid.clone())
            .or_insert_with(|| short_artifact_label(bundle, aid))
            .clone();
        for sec in sections.iter() {
            entries.push(SearchEntry {
                display: sec.name.to_string(),
                chip: format!("section · {alabel}"),
                kind_glyph: "▤",
                jump: SearchJump::SectionMap { artifact: aid.clone() },
            });
        }
    }

    // Strings from data sections.
    let mut string_count: usize = 0;
    const MAX_STRINGS: usize = 20_000;
    for ((aid, name), ds) in bundle.data_sections.iter() {
        if ds.kind == NativeSectionKind::Bss
            || ds.kind == NativeSectionKind::Debug
            || ds.base == 0
        {
            continue;
        }
        let alabel = artifact_label
            .entry(aid.clone())
            .or_insert_with(|| short_artifact_label(bundle, aid))
            .clone();
        let bytes: &[u8] = ds.bytes.as_ref();
        let mut i = 0;
        while i < bytes.len() {
            if !is_printable(bytes[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && is_printable(bytes[i]) {
                i += 1;
            }
            let end = i;
            let nul_terminated = i < bytes.len() && bytes[i] == 0;
            if !nul_terminated {
                continue;
            }
            let len = end - start;
            if len < 4 {
                continue;
            }
            let s = match std::str::from_utf8(&bytes[start..end]) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            };
            let addr = ds.base + start as u64;
            entries.push(SearchEntry {
                display: s,
                chip: format!("string · {} · {}", name, alabel),
                kind_glyph: "\"",
                jump: SearchJump::Hex {
                    artifact: aid.clone(),
                    section: name.clone(),
                    addr,
                },
            });
            string_count += 1;
            if string_count >= MAX_STRINGS {
                break;
            }
        }
        if string_count >= MAX_STRINGS {
            break;
        }
    }

    SearchIndex { entries }
}

fn is_printable(b: u8) -> bool {
    (0x20..=0x7e).contains(&b) || b == b'\t'
}

fn short_artifact_label(bundle: &LoadedBundle, aid: &glass_db::ArtifactId) -> String {
    for (i, k) in bundle.kinds.iter().enumerate() {
        let matches = match k {
            LeafKind::Listing { artifact, .. } => artifact == aid,
            LeafKind::Hex { artifact, .. } => artifact == aid,
            LeafKind::SectionMap { artifact } => artifact == aid,
            _ => false,
        };
        if matches {
            if let Some(label) = bundle.labels.get(i) {
                let s = label.as_ref();
                if let Some(prefix) = s.strip_suffix(" (overview)") {
                    return prefix.to_string();
                }
                return s.to_string();
            }
        }
    }
    aid.to_string()
}

pub fn jni_to_dotted(jni: &str) -> String {
    let trimmed = jni.strip_prefix('L').unwrap_or(jni);
    let trimmed = trimmed.strip_suffix(';').unwrap_or(trimmed);
    trimmed.replace('/', ".")
}
