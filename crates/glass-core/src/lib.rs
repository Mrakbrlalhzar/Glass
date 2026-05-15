//! glass-core: shared types that don't belong to any single backend.
//!
//! Architecture-specific loading lives in `glass-arch-arm64` and
//! `glass-arch-dex`; bundle handling lives in `glass-mobile`. This crate
//! intentionally stays small — it's the place to put types the DB, UI,
//! and script runtime all need to agree on (addresses, IDs, source kinds).

/// Where a piece of code came from. Mobile apps routinely contain both
/// kinds in one project (Android: DEX + native .so; iOS: ObjC + Swift).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeKind {
    /// Dalvik bytecode from a `.dex`.
    Dex,
    /// AArch64 machine code from a Mach-O or ELF.
    Arm64,
}
