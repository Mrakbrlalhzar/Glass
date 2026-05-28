//! `search` verb — case-insensitive substring match across native
//! symbols, DEX class names, and DEX method / field names.
//!
//! Scoped down from the GUI's full search index: we don't index
//! strings from data sections here (use `glass strings` for that)
//! and we don't carry jump-target metadata (consumers can call the
//! specific verb — `symbol-at`, `smali`, etc — to drill in).

use serde::Serialize;

use crate::bundle::Bundle;

#[derive(Serialize, Debug, Clone)]
pub struct SearchResult {
    pub query: String,
    pub total: usize,
    pub shown: usize,
    pub hits: Vec<SearchHit>,
}

#[derive(Serialize, Debug, Clone)]
pub struct SearchHit {
    pub kind: &'static str,
    pub label: String,
    /// Extra info: artifact label / class JNI / etc — for disambiguation.
    pub context: String,
    /// Best-effort jump target: hex address for native symbols, JNI
    /// for DEX classes, `Class;->name` for DEX methods/fields.
    pub jump: String,
}

impl Bundle {
    /// Substring-search across symbol + DEX names. Case-insensitive.
    pub fn search(&self, query: &str, limit: Option<usize>) -> SearchResult {
        let q = query.to_lowercase();
        let mut hits: Vec<SearchHit> = Vec::new();

        // Native symbols (one symbol map per artifact, lazy-build).
        for art in &self.artifacts {
            let symbols = glass_arch_arm::SymbolMap::build(&art.binary.container);
            for sym in symbols.iter() {
                let hay = sym.display_name.to_lowercase();
                if hay.contains(&q) {
                    hits.push(SearchHit {
                        kind: "symbol",
                        label: sym.display_name.clone(),
                        context: art.label.clone(),
                        jump: format!("0x{:x}", sym.address),
                    });
                }
            }
        }

        // DEX classes / methods / fields.
        for class in &self.dex_classes {
            let java = class.name.as_java_type();
            let jni = class.name.as_jni_type();
            if java.to_lowercase().contains(&q) || jni.to_lowercase().contains(&q) {
                hits.push(SearchHit {
                    kind: "class",
                    label: java.clone(),
                    context: jni.clone(),
                    jump: jni.clone(),
                });
            }
            for m in &class.methods {
                if m.name.to_lowercase().contains(&q) {
                    hits.push(SearchHit {
                        kind: "method",
                        label: format!("{}.{}", java, m.name),
                        context: m.signature.to_jni(),
                        jump: format!(
                            "{}->{}{}",
                            jni,
                            m.name,
                            m.signature.to_jni()
                        ),
                    });
                }
            }
            for f in &class.fields {
                if f.name.to_lowercase().contains(&q) {
                    hits.push(SearchHit {
                        kind: "field",
                        label: format!("{}.{}", java, f.name),
                        context: f.signature.to_jni(),
                        jump: format!("{}->{}:{}", jni, f.name, f.signature.to_jni()),
                    });
                }
            }
        }

        let total = hits.len();
        if let Some(cap) = limit {
            hits.truncate(cap);
        }
        SearchResult {
            query: query.to_string(),
            total,
            shown: hits.len(),
            hits,
        }
    }
}
