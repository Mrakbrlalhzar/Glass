//! CFG verbs — function CFG, call sites.

use anyhow::{Context, Result};
use glass_arch_arm64::SymbolMap;
use serde::Serialize;

use crate::bundle::Bundle;

#[derive(Serialize, Debug, Clone)]
pub struct CfgResult {
    pub artifact: String,
    pub function: String,
    pub entry_address: String,
    pub end_address: String,
    pub blocks: Vec<CfgBlock>,
    pub edges: Vec<CfgEdge>,
}

#[derive(Serialize, Debug, Clone)]
pub struct CfgBlock {
    pub id: usize,
    pub start_address: String,
    pub end_address: String,
    pub rank: usize,
    pub x: f32,
    pub instruction_count: usize,
    pub call_count: usize,
    pub exits_function: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct CfgEdge {
    pub from: usize,
    pub to: usize,
    pub kind: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct CallSiteInfo {
    pub site_address: String,
    pub target_address: Option<String>,
    pub target_name: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct CallsFromResult {
    pub function: String,
    pub entry_address: String,
    pub calls: Vec<CallSiteInfo>,
}

impl Bundle {
    /// Build the CFG for the function at `func_ref` (hex address or
    /// symbol name) in `artifact_ref`.
    pub fn cfg(&self, artifact_ref: &str, func_ref: &str) -> Result<CfgResult> {
        let art = self
            .artifacts
            .iter()
            .find(|a| {
                a.label == artifact_ref
                    || a.id.to_string().starts_with(artifact_ref)
            })
            .with_context(|| format!("no artifact matches {artifact_ref:?}"))?;
        let symbols = SymbolMap::build(&art.binary.container);
        let entry_addr = resolve_func_ref(&symbols, func_ref)
            .with_context(|| format!("no function for {func_ref:?}"))?;
        let cfg = glass_arch_arm64::build_function_cfg(
            &art.binary.container,
            &symbols,
            entry_addr,
        )
        .with_context(|| format!("no function at 0x{entry_addr:x}"))?;
        let function = symbols
            .at(entry_addr)
            .map(|s| s.display_name.clone())
            .unwrap_or_else(|| format!("sub_{entry_addr:x}"));
        let blocks = cfg
            .blocks
            .iter()
            .zip(cfg.layout.iter())
            .map(|(b, layout)| CfgBlock {
                id: b.id.0,
                start_address: format!("0x{:x}", b.start_addr),
                end_address: format!("0x{:x}", b.end_addr),
                rank: layout.rank,
                x: layout.x,
                instruction_count: b.instructions.len(),
                call_count: b.calls.len(),
                exits_function: b.exits_function,
            })
            .collect();
        let edges = cfg
            .edges
            .iter()
            .map(|e| CfgEdge {
                from: e.from.0,
                to: e.to.0,
                kind: format!("{:?}", e.kind),
            })
            .collect();
        Ok(CfgResult {
            artifact: art.id.to_string(),
            function,
            entry_address: format!("0x{:x}", entry_addr),
            end_address: format!("0x{:x}", cfg.end_addr),
            blocks,
            edges,
        })
    }

    /// List every call site in the function — useful for "what does
    /// this function depend on?" analyses without rebuilding the
    /// listing rows.
    pub fn calls_from(
        &self,
        artifact_ref: &str,
        func_ref: &str,
    ) -> Result<CallsFromResult> {
        let art = self
            .artifacts
            .iter()
            .find(|a| {
                a.label == artifact_ref
                    || a.id.to_string().starts_with(artifact_ref)
            })
            .with_context(|| format!("no artifact matches {artifact_ref:?}"))?;
        let symbols = SymbolMap::build(&art.binary.container);
        let entry_addr = resolve_func_ref(&symbols, func_ref)
            .with_context(|| format!("no function for {func_ref:?}"))?;
        let cfg = glass_arch_arm64::build_function_cfg(
            &art.binary.container,
            &symbols,
            entry_addr,
        )
        .with_context(|| format!("no function at 0x{entry_addr:x}"))?;
        let function = symbols
            .at(entry_addr)
            .map(|s| s.display_name.clone())
            .unwrap_or_else(|| format!("sub_{entry_addr:x}"));
        let mut calls = Vec::new();
        for b in &cfg.blocks {
            for c in &b.calls {
                let target_name = c
                    .target_addr
                    .and_then(|t| symbols.covering(t))
                    .map(|s| s.display_name.clone());
                calls.push(CallSiteInfo {
                    site_address: format!("0x{:x}", c.site_addr),
                    target_address: c.target_addr.map(|t| format!("0x{t:x}")),
                    target_name,
                });
            }
        }
        Ok(CallsFromResult {
            function,
            entry_address: format!("0x{:x}", entry_addr),
            calls,
        })
    }
}

fn resolve_func_ref(symbols: &SymbolMap, needle: &str) -> Option<u64> {
    // Hex address?
    if let Some(stripped) = needle.strip_prefix("0x") {
        if let Ok(v) = u64::from_str_radix(stripped, 16) {
            return Some(v);
        }
    }
    if let Ok(v) = u64::from_str_radix(needle, 16) {
        return Some(v);
    }
    // Else symbol name — match display_name or raw name exactly.
    symbols
        .iter()
        .find(|s| s.display_name == needle || s.name == needle)
        .map(|s| s.address)
}
