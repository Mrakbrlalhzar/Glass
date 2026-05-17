//! Merged symbol map for an AArch64 artifact.
//!
//! Combines three sources, in priority order:
//!   1. Symbol table (`Container::defined_symbols()` function-kind entries).
//!   2. DWARF subprograms (already lifted into `Container::dwarf.functions`).
//!   3. `.eh_frame` FDEs parsed with gimli — gives function extents even
//!      when the symtab has been stripped. FDE-only entries are named
//!      `sub_<hex>` since the FDE alone doesn't carry a name.
//!
//! Deduplication is by start address. Highest-priority source wins the
//! `name`; the `sources` bitset records every provider that contributed.

use std::collections::BTreeMap;

use armv8_encode::container::Container;
use gimli::{BaseAddresses, CieOrFde, EhFrame, LittleEndian, UnwindSection};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Object,
    Other,
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
    pub struct SymbolSources: u8 {
        const SYMTAB    = 0b001;
        const DWARF     = 0b010;
        const EH_FRAME  = 0b100;
    }
}

#[derive(Clone, Debug)]
pub struct Symbol {
    /// Raw name as it appears in the binary's symbol table — the
    /// stable identifier for annotations and persistence.
    pub name: String,
    /// Human-readable form. For Itanium-mangled C++ / Rust / Swift
    /// names this is the demangled string; for plain names it's
    /// identical to `name`.
    pub display_name: String,
    pub address: u64,
    pub size: u64,
    pub kind: SymbolKind,
    pub sources: SymbolSources,
}

/// Run `symbolic-demangle` over `raw`, returning the demangled form
/// when it differs from the input. Falls back to the raw name on any
/// error so callers can use this unconditionally.
pub fn demangle(raw: &str) -> String {
    use symbolic_common::{Language, Name, NameMangling};
    use symbolic_demangle::{Demangle, DemangleOptions};
    // Mangled inputs always start with a sigil — `_Z` (Itanium C++),
    // `__Z` (Mach-O variant), `_R` (Rust), or `_$LT$` (older Rust).
    // Skip work entirely for names that obviously aren't mangled.
    if !raw.starts_with('_') {
        return raw.to_string();
    }
    let name = Name::new(raw, NameMangling::Mangled, Language::Unknown);
    name.demangle(DemangleOptions::name_only())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| raw.to_string())
}

#[derive(Clone, Debug, Default)]
pub struct SymbolMap {
    by_address: BTreeMap<u64, Symbol>,
}

impl SymbolMap {
    pub fn build(container: &Container) -> Self {
        let mut by_address: BTreeMap<u64, Symbol> = BTreeMap::new();

        // (1) Symbol table — highest priority.
        for sym in container.defined_symbols() {
            let kind = match sym.kind {
                armv8_encode::container::SymbolKind::Function => SymbolKind::Function,
                armv8_encode::container::SymbolKind::Object => SymbolKind::Object,
                _ => SymbolKind::Other,
            };
            // Filter to "code-shaped" entries — skip section/file
            // symbols that just clutter the map.
            if !matches!(kind, SymbolKind::Function | SymbolKind::Object) {
                continue;
            }
            let display_name = demangle(&sym.name);
            insert_or_merge(
                &mut by_address,
                Symbol {
                    name: sym.name.clone(),
                    display_name,
                    address: sym.address,
                    size: sym.size,
                    kind,
                    sources: SymbolSources::SYMTAB,
                },
            );
        }

        // (2) DWARF subprograms — fill gaps left by the symtab.
        if let Some(dwarf) = &container.dwarf {
            for f in &dwarf.functions {
                let display_name = demangle(&f.name);
                insert_or_merge(
                    &mut by_address,
                    Symbol {
                        name: f.name.clone(),
                        display_name,
                        address: f.address,
                        size: f.size,
                        kind: SymbolKind::Function,
                        sources: SymbolSources::DWARF,
                    },
                );
            }
        }

        // (3) .eh_frame FDEs — fill remaining gaps. Parser lives at the
        // bottom of this file.
        for ent in parse_eh_frame_fdes(container) {
            let raw = format!("sub_{:x}", ent.address);
            insert_or_merge(
                &mut by_address,
                Symbol {
                    display_name: raw.clone(),
                    name: raw,
                    address: ent.address,
                    size: ent.size,
                    kind: SymbolKind::Function,
                    sources: SymbolSources::EH_FRAME,
                },
            );
        }

        // (4) Synthetic `<name>@plt` entries for every PLT stub. ELF
        // PLT slots are not in the static symbol table; armv8-encode
        // already pairs each undefined `.dynsym` entry with its
        // corresponding PLT stub address in `elf_image.plt_stubs`.
        // We surface those as function symbols so the listing renders
        // a symbol header at the stub and any `bl <stub>` call shows
        // the demangled extern name.
        if let Some(image) = container.elf_image.as_ref() {
            // PLT stubs on AArch64 are 16 bytes per slot. We use that
            // as the synthetic symbol's size so `covering` works.
            const AARCH64_PLT_SLOT: u64 = 16;
            for (sym_id, plt_addr) in &image.plt_stubs {
                let Some(dyn_sym) = container.symbols.get(sym_id.0) else {
                    continue;
                };
                let imported = &dyn_sym.name;
                let raw = format!("{imported}@plt");
                let display_name = format!("{}@plt", demangle(imported));
                insert_or_merge(
                    &mut by_address,
                    Symbol {
                        name: raw,
                        display_name,
                        address: *plt_addr,
                        size: AARCH64_PLT_SLOT,
                        kind: SymbolKind::Function,
                        sources: SymbolSources::SYMTAB,
                    },
                );
            }
        }

        // (5) Synthetic `<name>@stubs` entries for every Mach-O
        // `__stubs` slot — the Mach-O equivalent of ELF's PLT.
        // armv8-encode pairs each entry with the symbol-table entry
        // it binds to via the indirect symbol table.
        if let Some(image) = container.macho_image.as_ref() {
            // arm64 stub entries are 12 bytes (`adrp`/`ldr`/`br`).
            // The actual stride lives on the section's `reserved2`
            // field, but armv8-encode has already used that to
            // compute the per-entry address; here we just want a
            // sensible synthetic size for `covering` lookups.
            const AARCH64_STUB_SLOT: u64 = 12;
            for (sym_id, stub_addr) in &image.stubs {
                let Some(dyn_sym) = container.symbols.get(sym_id.0) else {
                    continue;
                };
                let imported = &dyn_sym.name;
                let raw = format!("{imported}@stubs");
                let display_name = format!("{}@stubs", demangle(imported));
                insert_or_merge(
                    &mut by_address,
                    Symbol {
                        name: raw,
                        display_name,
                        address: *stub_addr,
                        size: AARCH64_STUB_SLOT,
                        kind: SymbolKind::Function,
                        sources: SymbolSources::SYMTAB,
                    },
                );
            }
        }

        SymbolMap { by_address }
    }

    pub fn len(&self) -> usize {
        self.by_address.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_address.is_empty()
    }

    /// Iterate every symbol, by address.
    pub fn iter(&self) -> impl Iterator<Item = &Symbol> {
        self.by_address.values()
    }

    /// O(log N) lookup of the symbol that *starts* at `addr`. The
    /// callsite that wants "is there a symbol header here" should
    /// use this — `iter().find(...)` over a 50k-entry map runs
    /// O(N) per call which dominates a full-section disassembly.
    pub fn at(&self, addr: u64) -> Option<&Symbol> {
        self.by_address.get(&addr)
    }

    /// Symbols in `[start, end)`. Order is by address.
    pub fn in_range(&self, start: u64, end: u64) -> impl Iterator<Item = &Symbol> {
        self.by_address.range(start..end).map(|(_, s)| s)
    }

    /// The symbol whose extent contains `addr`, if any. Falls back to the
    /// nearest preceding symbol — useful in stripped binaries where
    /// `size` is zero and the FDE just gives us start points.
    pub fn covering(&self, addr: u64) -> Option<&Symbol> {
        if let Some((_, sym)) = self.by_address.range(..=addr).next_back() {
            // If the symbol has a known size, only return it when the
            // address really falls inside.
            if sym.size == 0 || addr < sym.address + sym.size {
                return Some(sym);
            }
        }
        None
    }
}

fn insert_or_merge(map: &mut BTreeMap<u64, Symbol>, new: Symbol) {
    match map.get_mut(&new.address) {
        None => {
            map.insert(new.address, new);
        }
        Some(existing) => {
            // Sources bitset always accumulates.
            existing.sources |= new.sources;
            // Higher-priority provider wins the name + kind. Symbol
            // priority is the order in which `build` calls this fn, so
            // we only overwrite if the existing entry has *no* current
            // info from a higher tier. The simplest correct rule: keep
            // the name we already have (first writer wins), and only
            // promote `size` if it was zero.
            if existing.size == 0 && new.size != 0 {
                existing.size = new.size;
            }
            if matches!(existing.kind, SymbolKind::Other) {
                existing.kind = new.kind;
            }
        }
    }
}

// ---- .eh_frame FDE extraction ----------------------------------------------

struct FdeEntry {
    address: u64,
    size: u64,
}

fn parse_eh_frame_fdes(container: &Container) -> Vec<FdeEntry> {
    // Find an `.eh_frame` section by name. ELF: `.eh_frame`. Mach-O:
    // `__eh_frame` (in `__TEXT,__eh_frame`). We match on both since
    // armv8-encode preserves the original name and section_kind for
    // these is `Other`.
    let Some(eh) = container
        .sections
        .iter()
        .find(|s| s.name == ".eh_frame" || s.name == "__eh_frame")
    else {
        return Vec::new();
    };
    if eh.bytes.is_empty() {
        return Vec::new();
    }
    // `BaseAddresses::default()` is fine for finding FDE pc_begin —
    // we just need the relative addresses gimli reports. For pc-relative
    // encodings (the common case in shipped ELF), we need eh_frame
    // base = its load address; without it gimli can't add the offset.
    let bases = BaseAddresses::default().set_eh_frame(eh.address);
    // Endianness: AArch64 is always little-endian, both for ELF and
    // Mach-O. armv8-encode is AArch64-only, so this is safe.
    let frame = EhFrame::new(&eh.bytes, LittleEndian);
    let mut entries = frame.entries(&bases);

    let mut out = Vec::new();
    loop {
        let parsed = match entries.next() {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("eh_frame parse stopped: {e}");
                break;
            }
        };
        if let CieOrFde::Fde(partial) = parsed {
            match partial.parse(EhFrame::cie_from_offset) {
                Ok(fde) => {
                    out.push(FdeEntry {
                        address: fde.initial_address(),
                        size: fde.len(),
                    });
                }
                Err(e) => {
                    tracing::debug!("FDE parse failed: {e}");
                }
            }
        }
    }
    out
}
