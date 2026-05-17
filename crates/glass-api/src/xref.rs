//! Xref verbs — native callers, DEX callers, DEX field refs.
//!
//! These don't cache on the bundle handle: the CLI is one-shot
//! (process exits after the call), so paying a per-query build is
//! the simpler path. The xref builds reuse the same algorithms the
//! GUI runs in `glass_ui::xref` — direct branches + ADRP/ADD pairs
//! for native, smali-line scanning for DEX.

use anyhow::{Context, Result};
use armv8_encode::isa::aarch64::{
    self, Aarch64Mnemonic, DecodedInstruction, DecodedOperand, RegisterClass,
};
use glass_arch_arm64::SymbolMap;
use serde::Serialize;
use smali::smali_ops::{DexOp, MethodRef};
use smali::types::SmaliOp;

use crate::bundle::Bundle;

#[derive(Serialize, Debug, Clone)]
pub struct XrefResult {
    pub artifact: String,
    /// The address we looked up, hex-formatted.
    pub target_address: String,
    /// Symbol covering the target, if any.
    pub target_symbol: Option<String>,
    pub sites: Vec<XrefSite>,
}

#[derive(Serialize, Debug, Clone)]
pub struct XrefSite {
    pub address: String,
    /// Symbol covering the site, if any.
    pub function: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct DexCallersResult {
    pub method_key: String,
    pub callers: Vec<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct FieldRefsResult {
    pub field_ref: String,
    pub methods: Vec<String>,
}

impl Bundle {
    /// Native-code callers / address-takes referencing `target_addr`
    /// inside `artifact_ref`'s text section(s). Includes direct
    /// branches and resolved ADRP/ADD pairs.
    pub fn xref_addr(
        &self,
        artifact_ref: &str,
        target_addr: u64,
    ) -> Result<XrefResult> {
        let art = self
            .artifacts
            .iter()
            .find(|a| {
                a.label == artifact_ref
                    || a.id.to_string().starts_with(artifact_ref)
            })
            .with_context(|| format!("no artifact matches {artifact_ref:?}"))?;
        let symbols = SymbolMap::build(&art.binary.container);
        let sites = collect_native_sites(&art.binary.container, target_addr);
        let target_symbol = symbols
            .covering(target_addr)
            .map(|s| s.display_name.clone());
        let sites = sites
            .into_iter()
            .map(|addr| XrefSite {
                address: format!("0x{addr:x}"),
                function: symbols
                    .covering(addr)
                    .map(|s| s.display_name.clone()),
            })
            .collect();
        Ok(XrefResult {
            artifact: art.id.to_string(),
            target_address: format!("0x{target_addr:x}"),
            target_symbol,
            sites,
        })
    }

    /// Same as `xref_addr` but accepts a symbol name (display or
    /// raw). Convenience wrapper for "who calls X?".
    pub fn callers(
        &self,
        artifact_ref: &str,
        symbol_name: &str,
    ) -> Result<XrefResult> {
        let art = self
            .artifacts
            .iter()
            .find(|a| {
                a.label == artifact_ref
                    || a.id.to_string().starts_with(artifact_ref)
            })
            .with_context(|| format!("no artifact matches {artifact_ref:?}"))?;
        let symbols = SymbolMap::build(&art.binary.container);
        let entry = symbols
            .iter()
            .find(|s| s.display_name == symbol_name || s.name == symbol_name)
            .with_context(|| format!("no symbol named {symbol_name:?}"))?;
        self.xref_addr(artifact_ref, entry.address)
    }

    /// Methods that `invoke-*` the given DEX method key. `method_key`
    /// is `Lclass;->name(descriptor)return` form.
    pub fn dex_callers(&self, method_key: &str) -> DexCallersResult {
        let mut callers = Vec::new();
        for class in &self.dex_classes {
            let class_jni = class.name.as_jni_type();
            for method in &class.methods {
                let caller_key = format!(
                    "{class_jni}->{name}{desc}",
                    name = method.name,
                    desc = method.signature.to_jni(),
                );
                let mut hit = false;
                for op in &method.ops {
                    if let SmaliOp::Op(dex) = op {
                        if let Some(m) = invoke_target(dex) {
                            if method_ref_key(m) == method_key {
                                hit = true;
                                break;
                            }
                        }
                    }
                }
                if hit {
                    callers.push(caller_key);
                }
            }
        }
        callers.sort();
        callers.dedup();
        DexCallersResult {
            method_key: method_key.to_string(),
            callers,
        }
    }

    /// Methods that read or write the given smali field ref
    /// (`Lclass;->name:Ltype;`).
    pub fn field_refs(&self, field_ref: &str) -> FieldRefsResult {
        let mut methods = Vec::new();
        for class in &self.dex_classes {
            let class_jni = class.name.as_jni_type();
            for method in &class.methods {
                let method_key = format!(
                    "{class_jni}->{name}{desc}",
                    name = method.name,
                    desc = method.signature.to_jni(),
                );
                let mut hit = false;
                for op in &method.ops {
                    if let SmaliOp::Op(dex) = op {
                        if let Some(f) = field_access(dex) {
                            if f == field_ref {
                                hit = true;
                                break;
                            }
                        }
                    }
                }
                if hit {
                    methods.push(method_key);
                }
            }
        }
        methods.sort();
        methods.dedup();
        FieldRefsResult {
            field_ref: field_ref.to_string(),
            methods,
        }
    }
}

fn collect_native_sites(
    container: &armv8_encode::container::Container,
    target: u64,
) -> Vec<u64> {
    use armv8_encode::container::SectionKind;
    let mut sites = Vec::new();
    for section in &container.sections {
        if !matches!(section.kind, SectionKind::Text) {
            continue;
        }
        let base = section.address;
        let bytes: &[u8] = &section.bytes;
        let n = bytes.len() / 4;
        let mut page_bases: [Option<u64>; 32] = [None; 32];
        for i in 0..n {
            let addr = base + (i as u64) * 4;
            let word = u32::from_le_bytes([
                bytes[i * 4],
                bytes[i * 4 + 1],
                bytes[i * 4 + 2],
                bytes[i * 4 + 3],
            ]);
            let Ok(insn) = aarch64::decode_instruction(addr, word) else {
                continue;
            };
            if let Some(t) = glass_arch_arm64::format::primary_address_operand(&insn) {
                if t == target {
                    sites.push(addr);
                }
            }
            if let Some((d, page)) = extract_adrp(&insn) {
                if (d as usize) < page_bases.len() {
                    page_bases[d as usize] = Some(page);
                }
            } else if let Some((_, _, t)) = extract_add_with_imm(&insn, &page_bases) {
                if t == target {
                    sites.push(addr);
                }
            } else if let Some(d) = dest_x_reg(&insn) {
                if (d as usize) < page_bases.len() {
                    page_bases[d as usize] = None;
                }
            }
        }
    }
    sites.sort_unstable();
    sites.dedup();
    sites
}

fn x_regs_of(insn: &DecodedInstruction) -> Vec<u8> {
    let mut out = Vec::with_capacity(insn.operands.len());
    for op in &insn.operands {
        if let DecodedOperand::Register(r) = op {
            if matches!(r.class, RegisterClass::X | RegisterClass::XOrSp) {
                out.push(r.index);
            }
        }
    }
    out
}

fn first_imm_of(insn: &DecodedInstruction) -> Option<i64> {
    for op in &insn.operands {
        match op {
            DecodedOperand::Immediate(v) => return Some(*v),
            DecodedOperand::UnsignedImmediate(v) => return Some(*v as i64),
            DecodedOperand::ShiftedImmediate(s) => {
                return Some(s.value.wrapping_shl(s.shift as u32))
            }
            _ => {}
        }
    }
    None
}

fn extract_adrp(insn: &DecodedInstruction) -> Option<(u8, u64)> {
    if insn.mnemonic != Aarch64Mnemonic::Adrp {
        return None;
    }
    let regs = x_regs_of(insn);
    let page = insn.operands.iter().find_map(|op| match op {
        DecodedOperand::PageTarget(a) => Some(*a),
        _ => None,
    });
    Some((*regs.first()?, page?))
}

fn extract_add_with_imm(
    insn: &DecodedInstruction,
    page_bases: &[Option<u64>; 32],
) -> Option<(u8, u8, u64)> {
    if insn.mnemonic != Aarch64Mnemonic::Add {
        return None;
    }
    let regs = x_regs_of(insn);
    if regs.len() < 2 {
        return None;
    }
    let d = regs[0];
    let s = regs[1];
    let base = page_bases.get(s as usize).copied().flatten()?;
    let imm = first_imm_of(insn)?;
    if imm < 0 {
        return None;
    }
    Some((d, s, base.wrapping_add(imm as u64)))
}

fn dest_x_reg(insn: &DecodedInstruction) -> Option<u8> {
    x_regs_of(insn).into_iter().next()
}

fn invoke_target(op: &DexOp) -> Option<&MethodRef> {
    Some(match op {
        DexOp::InvokeVirtual { method, .. }
        | DexOp::InvokeSuper { method, .. }
        | DexOp::InvokeDirect { method, .. }
        | DexOp::InvokeStatic { method, .. }
        | DexOp::InvokeInterface { method, .. }
        | DexOp::InvokeVirtualRange { method, .. }
        | DexOp::InvokeSuperRange { method, .. }
        | DexOp::InvokeDirectRange { method, .. }
        | DexOp::InvokeStaticRange { method, .. }
        | DexOp::InvokeInterfaceRange { method, .. } => method,
        _ => return None,
    })
}

fn method_ref_key(m: &MethodRef) -> String {
    format!("{}->{}{}", m.class, m.name, m.descriptor)
}

fn field_access(op: &DexOp) -> Option<String> {
    use smali::smali_ops::FieldRef;
    let fr: &FieldRef = match op {
        DexOp::IGet { field, .. }
        | DexOp::IGetWide { field, .. }
        | DexOp::IGetObject { field, .. }
        | DexOp::IGetBoolean { field, .. }
        | DexOp::IGetByte { field, .. }
        | DexOp::IGetChar { field, .. }
        | DexOp::IGetShort { field, .. }
        | DexOp::IPut { field, .. }
        | DexOp::IPutWide { field, .. }
        | DexOp::IPutObject { field, .. }
        | DexOp::IPutBoolean { field, .. }
        | DexOp::IPutByte { field, .. }
        | DexOp::IPutChar { field, .. }
        | DexOp::IPutShort { field, .. }
        | DexOp::SGet { field, .. }
        | DexOp::SGetWide { field, .. }
        | DexOp::SGetObject { field, .. }
        | DexOp::SGetBoolean { field, .. }
        | DexOp::SGetByte { field, .. }
        | DexOp::SGetChar { field, .. }
        | DexOp::SGetShort { field, .. }
        | DexOp::SPut { field, .. }
        | DexOp::SPutWide { field, .. }
        | DexOp::SPutObject { field, .. }
        | DexOp::SPutBoolean { field, .. }
        | DexOp::SPutByte { field, .. }
        | DexOp::SPutChar { field, .. }
        | DexOp::SPutShort { field, .. } => field,
        _ => return None,
    };
    Some(format!("{}->{}:{}", fr.class, fr.name, fr.descriptor))
}
