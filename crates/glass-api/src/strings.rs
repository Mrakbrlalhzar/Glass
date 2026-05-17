//! `strings` verb — printable-ASCII extraction from native sections.
//!
//! Strict NUL-terminated, ≥`min_len` printable bytes. The same
//! filter the GUI uses to populate the search index's "strings"
//! entries; pulling it into a dedicated verb makes it scriptable.

use anyhow::{Context, Result};
use armv8_encode::container::{Section, SectionKind};
use serde::Serialize;

use crate::bundle::Bundle;

#[derive(Serialize, Debug, Clone)]
pub struct StringsListing {
    pub artifact: String,
    pub total: usize,
    pub shown: usize,
    pub strings: Vec<StringHit>,
}

#[derive(Serialize, Debug, Clone)]
pub struct StringHit {
    pub address: String,
    pub section: String,
    pub value: String,
}

impl Bundle {
    /// Extract printable-ASCII NUL-terminated strings from data
    /// sections of `artifact_ref`. Skips text / debug / BSS and
    /// zero-base sections. `min_len` defaults to 4.
    pub fn strings(
        &self,
        artifact_ref: &str,
        min_len: Option<usize>,
        limit: Option<usize>,
    ) -> Result<StringsListing> {
        let art = self
            .artifacts
            .iter()
            .find(|a| {
                a.label == artifact_ref
                    || a.id.to_string().starts_with(artifact_ref)
            })
            .with_context(|| format!("no artifact matches {artifact_ref:?}"))?;
        let min = min_len.unwrap_or(4);
        let cap = limit.unwrap_or(usize::MAX);
        let mut all = Vec::new();
        for section in &art.binary.container.sections {
            if matches!(section.kind, SectionKind::Text | SectionKind::Debug) {
                continue;
            }
            if section.address == 0 || section.bytes.is_empty() {
                continue;
            }
            extract_from(section, min, &mut all);
        }
        let total = all.len();
        if all.len() > cap {
            all.truncate(cap);
        }
        Ok(StringsListing {
            artifact: art.id.to_string(),
            total,
            shown: all.len(),
            strings: all,
        })
    }
}

fn extract_from(section: &Section, min_len: usize, out: &mut Vec<StringHit>) {
    let bytes: &[u8] = &section.bytes;
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
        if len < min_len {
            continue;
        }
        let Ok(s) = std::str::from_utf8(&bytes[start..end]) else {
            continue;
        };
        out.push(StringHit {
            address: format!("0x{:x}", section.address + start as u64),
            section: section.name.clone(),
            value: s.to_string(),
        });
    }
}

fn is_printable(b: u8) -> bool {
    (0x20..=0x7e).contains(&b) || b == b'\t'
}
