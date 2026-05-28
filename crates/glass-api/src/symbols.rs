//! Symbol verbs — list, lookup-by-address, demangle.

use std::sync::Arc;

use glass_arch_arm::{Symbol, SymbolKind, SymbolMap, SymbolSources};
use serde::Serialize;

use crate::bundle::{Bundle, ParsedArtifact};

/// JSON-friendly projection of `glass_arch_arm::Symbol`. Differs
/// from the upstream type only in that address is a hex string and
/// `sources` is an explicit list of strings (the bitflags type
/// doesn't serialise as JSON cleanly).
#[derive(Serialize, Debug, Clone)]
pub struct SymbolInfo {
    pub address: String,
    pub size: u64,
    pub name: String,
    pub demangled: String,
    pub kind: SymbolKindName,
    pub sources: Vec<String>,
}

/// Mirror of `SymbolKind` with a stable JSON representation.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKindName {
    Function,
    Object,
    Other,
}

#[derive(Serialize, Debug, Clone)]
pub struct SymbolListing {
    pub artifact: String,
    pub total: usize,
    pub shown: usize,
    pub symbols: Vec<SymbolInfo>,
}

/// Filter options for `Bundle::symbols`.
#[derive(Default, Debug, Clone)]
pub struct SymbolQuery<'a> {
    pub artifact: Option<&'a str>,
    /// Substring filter (case-insensitive) over `demangled`.
    pub filter: Option<&'a str>,
    /// Only return symbols of this kind.
    pub kind: Option<SymbolKindName>,
    /// Cap on returned results. None = no cap.
    pub limit: Option<usize>,
}

impl Bundle {
    /// List symbols across one artifact (when `query.artifact` is
    /// set) or all artifacts. Returns one listing per artifact
    /// even when filtered to one — keeps the JSON shape stable.
    pub fn symbols(&self, query: SymbolQuery<'_>) -> Vec<SymbolListing> {
        let mut out = Vec::new();
        for a in &self.artifacts {
            if let Some(needle) = query.artifact {
                if a.label != needle && !a.id.to_string().starts_with(needle) {
                    continue;
                }
            }
            let sm = self.ensure_symbol_map(a);
            let total = sm.len();
            let mut symbols: Vec<SymbolInfo> = sm
                .iter()
                .filter(|s| {
                    query.kind.is_none_or(|k| symbol_kind_name(s.kind) == k)
                })
                .filter(|s| {
                    query.filter.is_none_or(|needle| {
                        let needle = needle.to_ascii_lowercase();
                        s.display_name.to_ascii_lowercase().contains(&needle)
                            || s.name.to_ascii_lowercase().contains(&needle)
                    })
                })
                .map(|s| SymbolInfo::from(s.clone()))
                .collect();
            symbols.sort_by(|a, b| a.address.cmp(&b.address));
            let shown = if let Some(cap) = query.limit {
                let n = symbols.len().min(cap);
                symbols.truncate(n);
                n
            } else {
                symbols.len()
            };
            out.push(SymbolListing {
                artifact: a.id.to_string(),
                total,
                shown,
                symbols,
            });
        }
        out
    }

    /// Look up the symbol covering `addr` in `artifact_ref` (label
    /// or hex-prefix). Returns the covering symbol if there is one,
    /// the at-address symbol if there's one starting exactly at
    /// `addr`, or None.
    pub fn symbol_at(
        &self,
        artifact_ref: &str,
        addr: u64,
    ) -> Option<SymbolInfo> {
        let a = self.artifacts.iter().find(|a| {
            a.label == artifact_ref || a.id.to_string().starts_with(artifact_ref)
        })?;
        let sm = self.ensure_symbol_map(a);
        sm.at(addr)
            .cloned()
            .or_else(|| sm.covering(addr).cloned())
            .map(SymbolInfo::from)
    }

    fn ensure_symbol_map(&self, art: &ParsedArtifact) -> Arc<SymbolMap> {
        if let Some(existing) = art.symbol_map.read().clone() {
            return existing;
        }
        let built = Arc::new(SymbolMap::build(&art.binary.container));
        *art.symbol_map.write() = Some(built.clone());
        built
    }
}

impl From<Symbol> for SymbolInfo {
    fn from(s: Symbol) -> Self {
        SymbolInfo {
            address: format!("0x{:x}", s.address),
            size: s.size,
            name: s.name,
            demangled: s.display_name,
            kind: symbol_kind_name(s.kind),
            sources: source_names(s.sources),
        }
    }
}

fn symbol_kind_name(k: SymbolKind) -> SymbolKindName {
    match k {
        SymbolKind::Function => SymbolKindName::Function,
        SymbolKind::Object => SymbolKindName::Object,
        SymbolKind::Other => SymbolKindName::Other,
    }
}

fn source_names(s: SymbolSources) -> Vec<String> {
    let mut out = Vec::new();
    if s.contains(SymbolSources::SYMTAB) {
        out.push("symtab".to_string());
    }
    if s.contains(SymbolSources::DWARF) {
        out.push("dwarf".to_string());
    }
    if s.contains(SymbolSources::EH_FRAME) {
        out.push("eh_frame".to_string());
    }
    out
}

/// Standalone demangler — no bundle required.
#[derive(Serialize, Debug, Clone)]
pub struct DemangleResult {
    pub input: String,
    pub demangled: String,
}

pub fn demangle(name: &str) -> DemangleResult {
    DemangleResult {
        input: name.to_string(),
        demangled: glass_arch_arm::demangle_symbol(name),
    }
}
