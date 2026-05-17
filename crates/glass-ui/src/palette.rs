//! Listing + smali colour palette.
//!
//! Single source of truth for the colours used by the listing, hex,
//! manifest, smali and CFG renderers. Centralising them here lets us
//! tweak the theme in one place and keeps the renderer modules
//! visually consistent.
//!
//! Note: this is a colour palette, not the command palette
//! (cmd-F overlay). That overlay lives in the main shell file.

use glass_arch_arm64::ChunkKind;

pub const COLOUR_ADDR: u32 = 0x8a8a92;
pub const COLOUR_BYTES: u32 = 0x676770;
pub const COLOUR_MNEMONIC: u32 = 0x6fc3df;
pub const COLOUR_REGISTER: u32 = 0xa8c5ff;
pub const COLOUR_IMMEDIATE: u32 = 0xf4a55a;
pub const COLOUR_ADDRESS_OP: u32 = 0xf3d27a;
pub const COLOUR_SHIFT: u32 = 0xb6b6c0;
pub const COLOUR_CONDITION: u32 = 0xc191ff;
pub const COLOUR_PUNCT: u32 = 0x808088;
pub const COLOUR_COMMENT: u32 = 0x6e9c5d;
pub const COLOUR_SYMBOL_HEADER: u32 = 0xfff39c;
pub const COLOUR_BB_SEPARATOR: u32 = 0x3a3a42;
pub const COLOUR_PLAIN: u32 = 0xd6d6d6;

// Smali-specific palette — reuses Register, Immediate, Punct, Plain
// from the listing palette to keep the two views consistent.
pub const COLOUR_DIRECTIVE: u32 = 0xff9c6e;
pub const COLOUR_MODIFIER: u32 = 0xc191ff;
pub const COLOUR_LABEL: u32 = 0xff8fc1;
pub const COLOUR_TYPE: u32 = 0xf3d27a;
pub const COLOUR_TYPE_EXTERNAL: u32 = 0x8c7a4a;
pub const COLOUR_STRING: u32 = 0xa5d678;

/// Subtle accent tint for the selected row. Brighter than the panel
/// background but dim enough not to fight the colour-coded chunks.
pub const COLOUR_ROW_SELECTED: u32 = 0x2e3245;

pub fn chunk_colour(kind: ChunkKind) -> u32 {
    use ChunkKind as K;
    match kind {
        K::Mnemonic => COLOUR_MNEMONIC,
        K::Register => COLOUR_REGISTER,
        K::Immediate => COLOUR_IMMEDIATE,
        K::Address => COLOUR_ADDRESS_OP,
        K::Shift => COLOUR_SHIFT,
        K::Condition => COLOUR_CONDITION,
        K::Punct => COLOUR_PUNCT,
        K::Plain => COLOUR_PLAIN,
        K::Directive => COLOUR_DIRECTIVE,
        K::Modifier => COLOUR_MODIFIER,
        K::Label => COLOUR_LABEL,
        K::Comment => COLOUR_COMMENT,
        K::Type => COLOUR_TYPE,
        K::String => COLOUR_STRING,
        // MethodName: colourwise this is a plain identifier. The
        // renderer wraps it in a clickable affordance separately
        // when the method ref resolves.
        K::MethodName => COLOUR_PLAIN,
    }
}
