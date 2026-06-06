//! ARM-family disassembly facade over `armv8-encode`.
//!
//! Originally an AArch64-only crate (`glass-arch-arm64`); now also
//! covers ARMv7 (A32) and Thumb (T32) for 32-bit ELF artifacts. The
//! [`facade::DecodedInsn`] enum gives the rest of Glass a single
//! ISA-neutral instruction type while keeping per-ISA bits available
//! when they're needed (encoding, AArch64-specific page-base fusion).
//!
//! Used for iOS Mach-O code, Android `lib*.so` native libraries, and
//! any other ARM ELF/Mach-O that flows through `glass-mobile`.

use std::path::Path;

use anyhow::{Context, Result};
use armv8_encode::container::Container;

pub mod arm_format;
pub mod cfg;
pub mod disasm;
pub mod facade;
pub mod format;
pub mod macho_fat;
pub mod objc_format;
pub mod swift_format;
pub mod symbol_map;
pub use cfg::{
    build_function_cfg, build_function_cfg_armv7_from_insns, build_function_cfg_from_bytes,
    BasicBlock, BlockEdge, BlockEdgeKind, BlockId, BlockLayout, CallSite, FunctionCfg,
    InstructionEntry,
};
pub use disasm::{disassemble_function_at, precompute_section_insns};
pub use facade::{DecodedInsn, FusionTarget, PageBaseTracker, RegKind, RegRef};
pub use format::{Chunk, ChunkKind};
pub use macho_fat::thin_slice_macho;
pub use symbol_map::{
    demangle as demangle_symbol, Symbol, SymbolKind, SymbolMap, SymbolSources,
};

pub struct Arm64Binary {
    pub path: std::path::PathBuf,
    pub bytes: Vec<u8>,
    pub container: Container,
}

impl Arm64Binary {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let bytes = std::fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_bytes(path, bytes)
    }

    /// Parse `bytes` as an AArch64 container. If the bytes are a fat
    /// Mach-O, slices down to the arm64 / arm64e arch first (other
    /// architectures are skipped — armv8-encode only decodes arm64).
    pub fn from_bytes(path: std::path::PathBuf, bytes: Vec<u8>) -> Result<Self> {
        // thin_slice_macho returns Ok(thin Mach-O bytes) for fat or
        // thin Mach-O inputs, and Err for non-Mach-O. ELF / other
        // formats simply fall through.
        let bytes = match thin_slice_macho(&bytes) {
            Ok(thin) => thin,
            Err(_) => bytes,
        };
        let container = Container::from_bytes(&bytes)
            .context("parsing native container (ELF/Mach-O)")?;
        // Note: armv8-encode's reader is deliberately lenient and
        // parses any ELF/Mach-O, tagging non-ARM machines (x86_64, …)
        // as `Architecture::Other`. We keep them — the file is still
        // inspectable — and let the UI route unsupported arches to the
        // hex viewer instead of the AArch64/ARMv7 disassembly. See
        // `glass_ui::loader::is_listable`.
        Ok(Self { path, bytes, container })
    }
}

pub struct Row {
    pub address: u64,
    pub bytes: [u8; 4],
    pub text: String,
}

pub fn linear_sweep(container: &Container) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for section in container.text_sections() {
        let base = section.address;
        let bytes = &section.bytes;
        for (i, chunk) in bytes.chunks_exact(4).enumerate() {
            let addr = base + (i as u64) * 4;
            let word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let text = match armv8_encode::isa::aarch64::decode_instruction(addr, word) {
                Ok(insn) => format!("{insn:?}"),
                Err(_) => format!(".word 0x{word:08x}"),
            };
            rows.push(Row {
                address: addr,
                bytes: [chunk[0], chunk[1], chunk[2], chunk[3]],
                text,
            });
        }
    }
    Ok(rows)
}
