//! AArch64 disassembly facade over `armv8-encode`.
//!
//! Used for iOS Mach-O code, Android `lib*.so` native libraries, and any
//! other AArch64 ELF/Mach-O that flows through `glass-mobile`.

use std::path::Path;

use anyhow::{Context, Result};
use armv8_encode::container::Container;

pub mod format;
pub mod symbol_map;
pub use format::{Chunk, ChunkKind};
pub use symbol_map::{Symbol, SymbolKind, SymbolMap, SymbolSources};

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

    pub fn from_bytes(path: std::path::PathBuf, bytes: Vec<u8>) -> Result<Self> {
        let container = Container::from_bytes(&bytes)
            .context("parsing AArch64 container (ELF/Mach-O)")?;
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
