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
        const SYMTAB    = 0b0001;
        const DWARF     = 0b0010;
        const EH_FRAME  = 0b0100;
        /// Synthesised from Objective-C `__objc_classlist` metadata
        /// — method IMPs named `-[Class selector:]` (instance) or
        /// `+[Class selector:]` (class). The Mach-O symtab usually
        /// doesn't carry these as proper symbols on Swift / mixed
        /// codebases, so the ObjC reader fills the gap.
        const OBJC      = 0b1000;
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
        //
        // Two things differ from the other symbol sources here:
        //
        //   1. The synthetic `@plt` name is *more* informative than
        //      whatever symtab entry might have landed at the same
        //      address (often a `$a`/`$t`/`$d` ARM mapping symbol or
        //      a generic stub mark) — so this pass *overwrites* any
        //      pre-existing non-`@plt` name on collisions instead of
        //      using the default first-writer-wins rule.
        //   2. PLT slot size is architecture-specific: AArch64 = 16,
        //      ARM = 12. Both ISAs use 4-byte instructions in the
        //      stub but ARM PLT slots have three instructions where
        //      AArch64 stubs have four (plus padding).
        if let Some(image) = container.elf_image.as_ref() {
            let plt_slot = match container.architecture {
                armv8_encode::container::Architecture::Aarch64 => 16u64,
                armv8_encode::container::Architecture::Arm => 12u64,
                _ => 16u64,
            };
            for (sym_id, plt_addr) in &image.plt_stubs {
                let Some(dyn_sym) = container.symbols.get(sym_id.0) else {
                    continue;
                };
                let imported = &dyn_sym.name;
                let raw = format!("{imported}@plt");
                let display_name = format!("{}@plt", demangle(imported));
                let plt_sym = Symbol {
                    name: raw,
                    display_name,
                    address: *plt_addr,
                    size: plt_slot,
                    kind: SymbolKind::Function,
                    sources: SymbolSources::SYMTAB,
                };
                // Replace whatever's there if the existing entry
                // isn't already a `@plt` form (i.e. don't clobber
                // ourselves on a duplicate iteration; do clobber
                // mapping / generic symbols that drowned us out).
                match by_address.get(plt_addr) {
                    Some(existing) if existing.name.ends_with("@plt") => {
                        // Keep the existing PLT entry; merge sources only.
                        insert_or_merge(&mut by_address, plt_sym);
                    }
                    _ => {
                        by_address.insert(*plt_addr, plt_sym);
                    }
                }
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

        // (6) Synthetic `-[Class selector:]` / `+[Class selector:]`
        // entries from Mach-O Objective-C metadata. Swift and
        // mixed iOS codebases rarely have IMP names in the
        // symtab; the ObjC reader walks `__objc_classlist` to
        // recover the canonical method names. Each method's `imp`
        // (when present) becomes a function symbol at that
        // address. We overwrite any prior entry whose name
        // doesn't already look like an ObjC selector — the
        // mangled-Swift name in the symtab is less useful than
        // the dotted ObjC form.
        if let Some(image) = container.macho_image.as_ref() {
            if let Ok(meta) = armv8_encode::container::read_objc_metadata(image) {
                for class in &meta.classes {
                    let raw_class = &class.name;
                    let pretty_class = demangle_objc_class(raw_class);
                    for m in &class.instance_methods {
                        if let Some(addr) = m.imp {
                            insert_objc_method(
                                &mut by_address,
                                addr,
                                format!("-[{raw_class} {}]", m.name),
                                format!("-[{pretty_class} {}]", m.name),
                            );
                        }
                    }
                    for m in &class.class_methods {
                        if let Some(addr) = m.imp {
                            insert_objc_method(
                                &mut by_address,
                                addr,
                                format!("+[{raw_class} {}]", m.name),
                                format!("+[{pretty_class} {}]", m.name),
                            );
                        }
                    }
                }
                // Categories add instance + class methods to an
                // existing class. The IMPs sit in the category
                // image, named via the category's own name (e.g.
                // `-[NSString(MyExt) lowercased]`).
                for cat in &meta.categories {
                    let raw_class = cat
                        .class_name
                        .clone()
                        .unwrap_or_else(|| "?".to_string());
                    let pretty_class = demangle_objc_class(&raw_class);
                    for m in &cat.instance_methods {
                        if let Some(addr) = m.imp {
                            insert_objc_method(
                                &mut by_address,
                                addr,
                                format!("-[{raw_class}({}) {}]", cat.name, m.name),
                                format!("-[{pretty_class}({}) {}]", cat.name, m.name),
                            );
                        }
                    }
                    for m in &cat.class_methods {
                        if let Some(addr) = m.imp {
                            insert_objc_method(
                                &mut by_address,
                                addr,
                                format!("+[{raw_class}({}) {}]", cat.name, m.name),
                                format!("+[{pretty_class}({}) {}]", cat.name, m.name),
                            );
                        }
                    }
                }
            }
            // `Err` outcomes are non-fatal: most commonly
            // `ChainedFixupsMissing` for legacy Mach-O that uses
            // `LC_DYLD_INFO_ONLY` instead of the modern format. Log
            // and continue — Glass still works without ObjC names.
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

    /// The symbol whose extent contains `addr`, if any.
    ///
    /// Sized symbols: returned when `addr` falls inside `[address,
    /// address + size)`. Size-0 *function* symbols (typically
    /// stripped binaries where symtab entries lack size info)
    /// fall back to the nearest preceding entry — useful for
    /// labelling instructions inside such a function. Size-0
    /// *data* symbols don't get that treatment: a zero-size
    /// `GCC_except_table0` shouldn't claim coverage of every
    /// address after it, which used to mislabel addresses in
    /// later sections (e.g. `__cstring`) as
    /// `GCC_except_table0+0x…`.
    pub fn covering(&self, addr: u64) -> Option<&Symbol> {
        if let Some((_, sym)) = self.by_address.range(..=addr).next_back() {
            if sym.size > 0 {
                if addr < sym.address + sym.size {
                    return Some(sym);
                }
            } else if matches!(sym.kind, SymbolKind::Function) {
                // Stripped-function fallback: only for code.
                return Some(sym);
            } else if sym.address == addr {
                // Point coverage for size-0 data symbols.
                return Some(sym);
            }
        }
        None
    }
}

/// Insert an ObjC-derived method symbol at `addr`. The ObjC
/// reader's name (`-[Class selector:]`) is strictly more
/// informative than whatever the symtab might carry at this IMP
/// address (typically a mangled Swift name or nothing), so we
/// overwrite any entry whose name doesn't already start with
/// `-[` or `+[` — those are pre-existing ObjC entries we keep.
/// The `sources` bitset still accumulates so callers can tell
/// the entry came from ObjC + something else.
///
/// `raw` is the stable persistence key (mangled class names
/// stay verbatim so annotation lookups survive demangler
/// changes); `display` is the user-facing form.
fn insert_objc_method(
    map: &mut BTreeMap<u64, Symbol>,
    addr: u64,
    raw: String,
    display: String,
) {
    let new = Symbol {
        name: raw,
        display_name: display,
        address: addr,
        size: 0,
        kind: SymbolKind::Function,
        sources: SymbolSources::OBJC,
    };
    match map.get_mut(&addr) {
        None => {
            map.insert(addr, new);
        }
        Some(existing) => {
            existing.sources |= SymbolSources::OBJC;
            // Promote kind if we don't already think it's a function.
            if !matches!(existing.kind, SymbolKind::Function) {
                existing.kind = SymbolKind::Function;
            }
            // Overwrite the name only if the existing one isn't
            // already in canonical ObjC form.
            let is_objc_shaped = existing.name.starts_with("-[")
                || existing.name.starts_with("+[");
            if !is_objc_shaped {
                existing.name = new.name;
                existing.display_name = new.display_name;
            }
        }
    }
}

/// Best-effort demangle for an ObjC class name. Plain ObjC
/// classes (`NSString`, `UIViewController`, `MyAppDelegate`)
/// pass through untouched. Swift classes registered with the
/// ObjC runtime have mangled names — the legacy form
/// `_TtC<len><module><len><class>` and the modern Swift ABI
/// form `_$s...<class>C`. `symbolic-demangle` knows both;
/// we just feed the raw name through and fall back to the
/// raw text if the demangler produces an empty / identical
/// string.
fn demangle_objc_class(raw: &str) -> String {
    if !raw.starts_with('_') {
        return raw.to_string();
    }
    let pretty = demangle(raw);
    if pretty.is_empty() || pretty == raw {
        raw.to_string()
    } else {
        pretty
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
