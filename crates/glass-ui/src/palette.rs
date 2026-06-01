//! Listing + smali colour palette.
//!
//! Thin adapter over `theme::current()` for the syntax-highlighting
//! callers. These return `u32` so the existing `rgb(COLOUR_X)` callsites
//! continue to work — they're just no-longer-`const` and now route
//! through the active theme.
//!
//! Note: this is a colour palette, not the command palette
//! (cmd-F overlay). That overlay lives in the main shell file.

use glass_arch_arm::ChunkKind;

use crate::theme;

fn rgba_to_u32(c: gpui::Rgba) -> u32 {
    let r = (c.r * 255.0).round() as u32;
    let g = (c.g * 255.0).round() as u32;
    let b = (c.b * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

macro_rules! colour {
    ($name:ident, $($path:ident).+) => {
        #[allow(non_snake_case)]
        pub fn $name() -> u32 {
            let t = theme::current();
            rgba_to_u32(t.$($path).+.rgba())
        }
    };
}

colour!(COLOUR_ADDR, disasm.address);
colour!(COLOUR_BYTES, disasm.bytes);
colour!(COLOUR_MNEMONIC, disasm.mnemonic);
colour!(COLOUR_REGISTER, disasm.register);
colour!(COLOUR_IMMEDIATE, disasm.immediate);
colour!(COLOUR_ADDRESS_OP, disasm.address_op);
colour!(COLOUR_SHIFT, disasm.shift);
colour!(COLOUR_CONDITION, disasm.condition);
colour!(COLOUR_PUNCT, disasm.punct);
colour!(COLOUR_COMMENT, disasm.comment);
colour!(COLOUR_SYMBOL_HEADER, disasm.symbol_header);
colour!(COLOUR_BB_SEPARATOR, disasm.bb_separator);
colour!(COLOUR_PLAIN, disasm.plain);

colour!(COLOUR_DIRECTIVE, smali.directive);
colour!(COLOUR_MODIFIER, smali.modifier);
colour!(COLOUR_LABEL, smali.label);
colour!(COLOUR_TYPE, smali.type_);
colour!(COLOUR_TYPE_EXTERNAL, smali.type_external);
colour!(COLOUR_STRING, smali.string);

colour!(COLOUR_ROW_SELECTED, state.row_selected);

pub fn chunk_colour(kind: ChunkKind) -> u32 {
    use ChunkKind as K;
    match kind {
        K::Mnemonic => COLOUR_MNEMONIC(),
        K::Register => COLOUR_REGISTER(),
        K::Immediate => COLOUR_IMMEDIATE(),
        K::Address => COLOUR_ADDRESS_OP(),
        K::Shift => COLOUR_SHIFT(),
        K::Condition => COLOUR_CONDITION(),
        K::Punct => COLOUR_PUNCT(),
        K::Plain => COLOUR_PLAIN(),
        K::Directive => COLOUR_DIRECTIVE(),
        K::Modifier => COLOUR_MODIFIER(),
        K::Label => COLOUR_LABEL(),
        K::Comment => COLOUR_COMMENT(),
        K::Type => COLOUR_TYPE(),
        K::String => COLOUR_STRING(),
        // MethodName / FieldName: colourwise these are plain
        // identifiers. The renderer wraps them in a clickable
        // affordance separately when the ref resolves.
        K::MethodName | K::FieldName => COLOUR_PLAIN(),
    }
}
