//! Automation-API verbs.
//!
//! One thin function per CLI subcommand: opens the bundle, runs
//! the glass-api call, emits via the output framework. No business
//! logic here — that's all in glass-api.

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;

use crate::output::{self, Envelope, Format};

pub fn inspect(path: PathBuf, format: Format) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        Ok(bundle.inspect())
    })?;
    output::emit(envelope, format, render_inspect)
}

pub fn artifacts(path: PathBuf, format: Format) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        Ok(bundle.artifacts())
    })?;
    output::emit(envelope, format, render_artifacts)
}

pub fn sections(
    path: PathBuf,
    artifact: Option<String>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        Ok(bundle.sections(artifact.as_deref()))
    })?;
    output::emit(envelope, format, render_sections)
}

pub fn binary_info(path: PathBuf, format: Format) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        Ok(bundle.binary_info())
    })?;
    output::emit(envelope, format, render_binary_info)
}

pub fn hash(path: PathBuf, format: Format) -> Result<()> {
    let envelope = output::measured(|| glass_api::hash_file(&path))?;
    output::emit(envelope, format, render_hash)
}

pub fn symbols(
    path: PathBuf,
    artifact: Option<String>,
    filter: Option<String>,
    kind: Option<glass_api::SymbolKindName>,
    limit: Option<usize>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        let query = glass_api::SymbolQuery {
            artifact: artifact.as_deref(),
            filter: filter.as_deref(),
            kind,
            limit,
        };
        Ok(bundle.symbols(query))
    })?;
    output::emit(envelope, format, render_symbols)
}

pub fn symbol_at(
    path: PathBuf,
    addr: String,
    artifact: String,
    format: Format,
) -> Result<()> {
    let addr = u64::from_str_radix(addr.trim_start_matches("0x"), 16)
        .map_err(|e| anyhow::anyhow!("bad address {addr:?}: {e}"))?;
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        Ok(bundle.symbol_at(&artifact, addr))
    })?;
    output::emit(envelope, format, render_symbol_at)
}

pub fn demangle(name: String, format: Format) -> Result<()> {
    let envelope = output::measured(|| Ok(glass_api::demangle(&name)))?;
    output::emit(envelope, format, render_demangle)
}

pub fn disasm(
    path: PathBuf,
    artifact: String,
    section: Option<String>,
    limit: Option<usize>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.disasm(&artifact, section.as_deref(), limit)
    })?;
    output::emit(envelope, format, render_disasm)
}

pub fn decode(word: String, addr: String, format: Format) -> Result<()> {
    let word_n = u32::from_str_radix(word.trim_start_matches("0x"), 16)
        .map_err(|e| anyhow::anyhow!("bad word {word:?}: {e}"))?;
    let addr_n = u64::from_str_radix(addr.trim_start_matches("0x"), 16)
        .map_err(|e| anyhow::anyhow!("bad addr {addr:?}: {e}"))?;
    let envelope =
        output::measured(|| Ok(glass_api::decode_word(word_n, addr_n)))?;
    output::emit(envelope, format, render_decode)
}

pub fn cfg_of(
    path: PathBuf,
    artifact: String,
    func: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.cfg(&artifact, &func)
    })?;
    output::emit(envelope, format, render_cfg)
}

pub fn calls_from(
    path: PathBuf,
    artifact: String,
    func: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.calls_from(&artifact, &func)
    })?;
    output::emit(envelope, format, render_calls_from)
}

pub fn classes(
    path: PathBuf,
    package: Option<String>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        Ok(bundle.classes(package.as_deref()))
    })?;
    output::emit(envelope, format, render_classes)
}

pub fn smali(path: PathBuf, class: String, format: Format) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.smali(&class)
    })?;
    output::emit(envelope, format, render_smali)
}

pub fn methods(path: PathBuf, class: String, format: Format) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.methods(&class)
    })?;
    output::emit(envelope, format, render_methods)
}

pub fn fields(path: PathBuf, class: String, format: Format) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.fields(&class)
    })?;
    output::emit(envelope, format, render_fields)
}

pub fn search(
    path: PathBuf,
    query: String,
    limit: Option<usize>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        Ok(bundle.search(&query, limit))
    })?;
    output::emit(envelope, format, render_search)
}

pub fn strings(
    path: PathBuf,
    artifact: String,
    min: Option<usize>,
    limit: Option<usize>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.strings(&artifact, min, limit)
    })?;
    output::emit(envelope, format, render_strings)
}

pub fn xref_addr(
    path: PathBuf,
    artifact: String,
    addr: String,
    format: Format,
) -> Result<()> {
    let addr_n = u64::from_str_radix(addr.trim_start_matches("0x"), 16)
        .map_err(|e| anyhow::anyhow!("bad address {addr:?}: {e}"))?;
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.xref_addr(&artifact, addr_n)
    })?;
    output::emit(envelope, format, render_xref)
}

pub fn callers(
    path: PathBuf,
    artifact: String,
    symbol: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.callers(&artifact, &symbol)
    })?;
    output::emit(envelope, format, render_xref)
}

pub fn dex_callers(
    path: PathBuf,
    method_key: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        Ok(bundle.dex_callers(&method_key))
    })?;
    output::emit(envelope, format, render_dex_callers)
}

pub fn field_refs(
    path: PathBuf,
    field_ref: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        Ok(bundle.field_refs(&field_ref))
    })?;
    output::emit(envelope, format, render_field_refs)
}

pub fn method_calls(
    path: PathBuf,
    class: String,
    method: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.method_calls(&class, &method)
    })?;
    output::emit(envelope, format, render_method_calls)
}

// ---- text renderers --------------------------------------------------------

fn render_inspect(
    data: &glass_api::BundleInspection,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{} ({})", data.label, data.kind)?;
    if let Some(id) = &data.bundle_id {
        writeln!(out, "  bundle id : {id}")?;
    }
    writeln!(out, "  source    : {}", data.source_path)?;
    writeln!(out, "  artifacts : {}", data.artifacts.len())?;
    for a in &data.artifacts {
        writeln!(
            out,
            "    {:<32} {:>10} bytes  {:<8} {} sections  ({})",
            a.label, a.size_bytes, a.architecture, a.section_count, a.id,
        )?;
    }
    Ok(())
}

fn render_artifacts(
    data: &Vec<glass_api::ArtifactInfo>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    for a in data {
        writeln!(
            out,
            "{}  {:>10} bytes  {:<8} {} sections  {}",
            a.id, a.size_bytes, a.architecture, a.section_count, a.label,
        )?;
    }
    Ok(())
}

fn render_sections(
    data: &Vec<glass_api::ArtifactSections>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    for art in data {
        writeln!(out, "{}", art.artifact)?;
        for s in &art.sections {
            writeln!(
                out,
                "  {:<24} {:>10} {:>10}  {:?}  ({} on disk)",
                s.name, s.address, s.size, s.kind, s.bytes_on_disk,
            )?;
        }
    }
    Ok(())
}

fn render_binary_info(
    data: &Vec<glass_api::BinaryInfo>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    for b in data {
        writeln!(
            out,
            "{}  {} {} ({})  {} bytes  {} sections  {} symbols",
            b.label,
            b.format,
            b.architecture,
            b.artifact,
            b.size_bytes,
            b.section_count,
            b.symbol_count_hint,
        )?;
    }
    Ok(())
}

fn render_hash(
    data: &glass_api::HashResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}  ({} bytes in {} ms)",
        data.artifact_id, data.size_bytes, data.duration_ms,
    )
}

fn render_symbols(
    data: &Vec<glass_api::SymbolListing>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    for listing in data {
        writeln!(
            out,
            "{}  {} of {} symbols",
            listing.artifact, listing.shown, listing.total,
        )?;
        for s in &listing.symbols {
            writeln!(
                out,
                "  {:>18}  {:>10}  {:?}  {}",
                s.address, s.size, s.kind, s.demangled,
            )?;
        }
    }
    Ok(())
}

fn render_symbol_at(
    data: &Option<glass_api::SymbolInfo>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    match data {
        Some(s) => writeln!(
            out,
            "{}  {} bytes  {:?}  {}  ({})",
            s.address, s.size, s.kind, s.demangled, s.name,
        ),
        None => writeln!(out, "(no symbol covers that address)"),
    }
}

fn render_demangle(
    data: &glass_api::DemangleResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    if data.demangled == data.input {
        writeln!(out, "{}  (not mangled)", data.input)
    } else {
        writeln!(out, "{}", data.demangled)
    }
}

fn render_disasm(
    data: &glass_api::DisasmListing,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{} {}  (base {}, {} of {} instructions)",
        data.artifact, data.section, data.base_address, data.shown, data.total_instructions,
    )?;
    for r in &data.rows {
        if let Some(sym) = &r.symbol {
            writeln!(out, "{sym}:")?;
        }
        let op = if r.operands.is_empty() {
            String::new()
        } else {
            format!(" {}", r.operands)
        };
        let comment = match &r.comment {
            Some(c) => format!("  ; {c}"),
            None => String::new(),
        };
        writeln!(out, "  {}  {}  {}{}{}", r.address, r.bytes, r.mnemonic, op, comment)?;
    }
    Ok(())
}

fn render_decode(
    data: &glass_api::DecodeResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let op = if data.operands.is_empty() {
        String::new()
    } else {
        format!(" {}", data.operands)
    };
    writeln!(out, "{} → {}{}", data.word, data.mnemonic, op)
}

fn render_cfg(
    data: &glass_api::CfgResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}  ({})  {} blocks  {} edges",
        data.function,
        data.entry_address,
        data.blocks.len(),
        data.edges.len(),
    )?;
    for b in &data.blocks {
        writeln!(
            out,
            "  block {:>3}  rank={}  x={:.1}  {}..{}  ({} insns, {} calls){}",
            b.id,
            b.rank,
            b.x,
            b.start_address,
            b.end_address,
            b.instruction_count,
            b.call_count,
            if b.exits_function { "  EXIT" } else { "" },
        )?;
    }
    for e in &data.edges {
        writeln!(out, "  edge {:>3} → {:<3}  {}", e.from, e.to, e.kind)?;
    }
    Ok(())
}

fn render_calls_from(
    data: &glass_api::CallsFromResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}  ({})  {} call sites",
        data.function,
        data.entry_address,
        data.calls.len(),
    )?;
    for c in &data.calls {
        let target = match (&c.target_address, &c.target_name) {
            (Some(a), Some(n)) => format!("{a}  {n}"),
            (Some(a), None) => a.clone(),
            (None, _) => "(indirect)".to_string(),
        };
        writeln!(out, "  {}  →  {}", c.site_address, target)?;
    }
    Ok(())
}

fn render_classes(
    data: &glass_api::ClassListing,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{} of {} classes", data.shown, data.total)?;
    for c in &data.classes {
        writeln!(
            out,
            "  {}  ({} fields, {} methods)  extends {}",
            c.java, c.field_count, c.method_count, c.super_class,
        )?;
    }
    Ok(())
}

fn render_smali(
    data: &glass_api::SmaliBody,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "# {}", data.class)?;
    out.write_all(data.smali.as_bytes())?;
    if !data.smali.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

fn render_methods(
    data: &glass_api::MethodListing,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{}  ({} methods)", data.class, data.methods.len())?;
    for m in &data.methods {
        let ctor = if m.constructor { "  <init>" } else { "" };
        writeln!(
            out,
            "  {}{}  ({} ops)  [{}]{}",
            m.name,
            m.descriptor,
            m.op_count,
            m.modifiers.join(" "),
            ctor,
        )?;
    }
    Ok(())
}

fn render_fields(
    data: &glass_api::FieldListing,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{}  ({} fields)", data.class, data.fields.len())?;
    for f in &data.fields {
        writeln!(
            out,
            "  {}:{}  [{}]",
            f.name,
            f.type_jni,
            f.modifiers.join(" "),
        )?;
    }
    Ok(())
}

fn render_method_calls(
    data: &glass_api::MethodCallsResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}->{}{}  ({} call sites)",
        data.class,
        data.method,
        data.descriptor,
        data.calls.len(),
    )?;
    for c in &data.calls {
        writeln!(
            out,
            "  {:<22}  {}->{}{}",
            c.kind, c.target_class, c.target_method, c.target_descriptor,
        )?;
    }
    Ok(())
}

fn render_xref(
    data: &glass_api::XrefResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let target = match &data.target_symbol {
        Some(s) => format!("{}  ({})", data.target_address, s),
        None => data.target_address.clone(),
    };
    writeln!(out, "{}  {} sites", target, data.sites.len())?;
    for s in &data.sites {
        match &s.function {
            Some(fname) => writeln!(out, "  {}  in {}", s.address, fname)?,
            None => writeln!(out, "  {}", s.address)?,
        }
    }
    Ok(())
}

fn render_dex_callers(
    data: &glass_api::DexCallersResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "callers of {}  ({})", data.method_key, data.callers.len())?;
    for c in &data.callers {
        writeln!(out, "  {c}")?;
    }
    Ok(())
}

fn render_field_refs(
    data: &glass_api::FieldRefsResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "users of {}  ({})", data.field_ref, data.methods.len())?;
    for m in &data.methods {
        writeln!(out, "  {m}")?;
    }
    Ok(())
}

fn render_search(
    data: &glass_api::SearchResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{:?}  {} of {} hits",
        data.query, data.shown, data.total,
    )?;
    for h in &data.hits {
        writeln!(
            out,
            "  [{:<6}]  {:<48}  {:<24}  → {}",
            h.kind, h.label, h.context, h.jump,
        )?;
    }
    Ok(())
}

fn render_strings(
    data: &glass_api::StringsListing,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}  {} of {} strings",
        data.artifact, data.shown, data.total,
    )?;
    for s in &data.strings {
        writeln!(out, "  {}  [{:<16}]  {:?}", s.address, s.section, s.value)?;
    }
    Ok(())
}

// Marker — `Envelope` referenced in the function signatures.
#[allow(dead_code)]
fn _envelope_marker(_: Envelope<()>) {}
