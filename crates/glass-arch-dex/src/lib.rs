//! DEX / smali facade over the `smali` crate.

use anyhow::{Context, Result};
use smali::dex::DexFile;
use smali::types::SmaliClass;

pub struct DexBinary {
    pub name: String,
    pub dex: DexFile,
    /// Raw `.dex` bytes — kept so we can hash this artifact for the
    /// persistence layer without re-reading from the APK zip.
    pub bytes: Vec<u8>,
    /// Lifted smali. Cached because lifting is non-trivial and the UI
    /// will re-read this on every click.
    classes: once_cell::sync::OnceCell<Vec<SmaliClass>>,
}

impl DexBinary {
    pub fn from_bytes(name: impl Into<String>, bytes: &[u8]) -> Result<Self> {
        let dex = DexFile::from_bytes(bytes).context("parsing .dex")?;
        Ok(Self {
            name: name.into(),
            dex,
            bytes: bytes.to_vec(),
            classes: once_cell::sync::OnceCell::new(),
        })
    }

    /// Lifted smali classes. Computed once on first call.
    pub fn classes(&self) -> Result<&[SmaliClass]> {
        self.classes
            .get_or_try_init(|| {
                self.dex
                    .to_smali()
                    .map_err(|e| anyhow::anyhow!("dex to_smali: {e:?}"))
            })
            .map(|v| v.as_slice())
    }
}
