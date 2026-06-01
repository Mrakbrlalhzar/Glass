//! Automation-API verbs.
//!
//! One thin function per CLI subcommand: opens the bundle, runs
//! the glass-api call, emits via the output framework. No business
//! logic here — that's all in glass-api.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context as _, Result};

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

pub fn types(
    path: PathBuf,
    artifact: Option<String>,
    package: Option<String>,
    kind: Option<String>,
    limit: usize,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let kind_filter = match kind.as_deref() {
            Some(s) => match glass_api::TypeKind::parse(s) {
                Some(k) => Some(k),
                None => anyhow::bail!(
                    "unknown --kind {s:?}: expected one of objc-class, objc-category, swift-class, swift-struct, swift-enum"
                ),
            },
            None => None,
        };
        let bundle = glass_api::open(&path)?;
        bundle.types(
            artifact.as_deref(),
            kind_filter,
            package.as_deref(),
            Some(limit),
        )
    })?;
    output::emit(envelope, format, render_types)
}

pub fn type_detail(
    path: PathBuf,
    artifact: String,
    name: String,
    raw: bool,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.type_detail(&artifact, &name, raw)
    })?;
    output::emit(envelope, format, render_type_detail)
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

pub fn annotations(path: PathBuf, format: Format) -> Result<()> {
    let envelope = output::measured(|| glass_api::annotations(&path))?;
    output::emit(envelope, format, render_annotations)
}

pub fn db_dump_v2(path: PathBuf, format: Format) -> Result<()> {
    let envelope = output::measured(|| glass_api::db_dump(&path))?;
    output::emit(envelope, format, render_db_dump)
}

pub fn set_rename(
    path: PathBuf,
    key_kind: String,
    key: String,
    method: Option<String>,
    name: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        glass_api::set_rename(
            &path,
            glass_api::AnnotationKeyArgs {
                kind: &key_kind,
                key: &key,
                method: method.as_deref(),
            },
            &name,
        )
    })?;
    output::emit(envelope, format, render_annotation_write)
}

pub fn set_comment(
    path: PathBuf,
    key_kind: String,
    key: String,
    method: Option<String>,
    text: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        glass_api::set_comment(
            &path,
            glass_api::AnnotationKeyArgs {
                kind: &key_kind,
                key: &key,
                method: method.as_deref(),
            },
            &text,
        )
    })?;
    output::emit(envelope, format, render_annotation_write)
}

pub fn set_colour(
    path: PathBuf,
    key_kind: String,
    key: String,
    method: Option<String>,
    rgba: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        glass_api::set_colour(
            &path,
            glass_api::AnnotationKeyArgs {
                kind: &key_kind,
                key: &key,
                method: method.as_deref(),
            },
            &rgba,
        )
    })?;
    output::emit(envelope, format, render_annotation_write)
}

pub fn clear_annotation(
    path: PathBuf,
    key_kind: String,
    key: String,
    method: Option<String>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        glass_api::clear_annotation(
            &path,
            glass_api::AnnotationKeyArgs {
                kind: &key_kind,
                key: &key,
                method: method.as_deref(),
            },
        )
    })?;
    output::emit(envelope, format, render_annotation_clear)
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

fn render_annotations(
    data: &glass_api::AnnotationsResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{}  ({} annotations)", data.artifact, data.total)?;
    for a in &data.annotations {
        write_entry_line(a, out)?;
    }
    Ok(())
}

fn write_entry_line(
    e: &glass_api::AnnotationEntry,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let mut facets: Vec<String> = Vec::new();
    if let Some(r) = &e.rename {
        facets.push(format!("rename={r}"));
    }
    if let Some(c) = &e.comment {
        facets.push(format!("comment={c:?}"));
    }
    if let Some(col) = &e.colour {
        facets.push(format!("colour={col}"));
    }
    writeln!(
        out,
        "  [{:<8}]  {:<40}  {}",
        e.key_kind,
        e.key,
        facets.join("  "),
    )
}

fn render_db_dump(
    data: &glass_api::DbDumpResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{}", data.bundle_id)?;
    writeln!(out, "  source: {}", data.source_path)?;
    match &data.record {
        None => writeln!(out, "  (no record)")?,
        Some(r) => {
            writeln!(out, "  label         : {}", r.label)?;
            writeln!(out, "  schema        : v{}", r.schema_version)?;
            writeln!(out, "  last_opened   : unix {}", r.last_opened_unix)?;
            writeln!(out, "  artifacts     : {}", r.artifact_count)?;
            writeln!(out, "  active_tab    : {:?}", r.active_tab)?;
            writeln!(out, "  open_tabs     : {}", r.open_tabs.len())?;
            for (i, t) in r.open_tabs.iter().enumerate() {
                writeln!(out, "    [{i}] {t}")?;
            }
            writeln!(out, "  expanded_paths: {}", r.expanded_paths.len())?;
            writeln!(out, "  source_path   : {:?}", r.source_path)?;
        }
    }
    Ok(())
}

fn render_annotation_write(
    data: &glass_api::AnnotationWriteResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{}", data.artifact)?;
    write_entry_line(&data.entry, out)
}

fn render_annotation_clear(
    data: &glass_api::AnnotationClearResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}  cleared  [{:<8}]  {}",
        data.artifact, data.key_kind, data.key,
    )
}

fn render_types(
    data: &glass_api::TypesResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{} of {} types", data.shown, data.total)?;
    if data.entries.is_empty() {
        return Ok(());
    }
    let kind_w = data.entries.iter().map(|e| kind_label(e.kind).len()).max().unwrap_or(4);
    let name_w = data.entries.iter().map(|e| e.name.len()).max().unwrap_or(4).min(60);
    let art_w = data.entries.iter().map(|e| e.artifact.len()).max().unwrap_or(8).min(32);
    let vaddr_w = data.entries.iter().map(|e| e.vaddr.len()).max().unwrap_or(10);
    writeln!(
        out,
        "  {:<kw$}  {:<nw$}  {:<aw$}  {:<vw$}  METHODS/FIELDS",
        "KIND",
        "NAME",
        "ARTIFACT",
        "VADDR",
        kw = kind_w,
        nw = name_w,
        aw = art_w,
        vw = vaddr_w,
    )?;
    for e in &data.entries {
        writeln!(
            out,
            "  {:<kw$}  {:<nw$}  {:<aw$}  {:<vw$}  {}/{}",
            kind_label(e.kind),
            e.name,
            e.artifact,
            e.vaddr,
            e.method_count,
            e.field_count,
            kw = kind_w,
            nw = name_w,
            aw = art_w,
            vw = vaddr_w,
        )?;
    }
    Ok(())
}

fn kind_label(k: glass_api::TypeKind) -> &'static str {
    match k {
        glass_api::TypeKind::ObjcClass => "objc-class",
        glass_api::TypeKind::ObjcCategory => "objc-category",
        glass_api::TypeKind::SwiftClass => "swift-class",
        glass_api::TypeKind::SwiftStruct => "swift-struct",
        glass_api::TypeKind::SwiftEnum => "swift-enum",
    }
}

fn render_type_detail(
    data: &glass_api::TypeDetail,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    match data {
        glass_api::TypeDetail::ObjcClass(c) => render_objc_class_detail(c, out),
        glass_api::TypeDetail::ObjcCategory(c) => render_objc_category_detail(c, out),
        glass_api::TypeDetail::SwiftClass(t) => render_swift_type_detail("class", t, out),
        glass_api::TypeDetail::SwiftStruct(t) => render_swift_type_detail("struct", t, out),
        glass_api::TypeDetail::SwiftEnum(t) => render_swift_type_detail("enum", t, out),
    }
}

fn render_objc_class_detail(
    c: &glass_api::ObjcClassDetail,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let sup = c.superclass.as_deref().unwrap_or("(none)");
    writeln!(out, "@interface {} : {}", c.name, sup)?;
    writeln!(
        out,
        "  // artifact {}  vaddr {}  flags 0x{:x}  size 0x{:x}",
        c.artifact, c.vaddr, c.flags, c.instance_size,
    )?;
    if !c.instance_methods.is_empty() {
        writeln!(out, "Instance methods:")?;
        for m in &c.instance_methods {
            write_objc_method(out, '-', &c.name, m)?;
        }
    }
    if !c.class_methods.is_empty() {
        writeln!(out, "Class methods:")?;
        for m in &c.class_methods {
            write_objc_method(out, '+', &c.name, m)?;
        }
    }
    if !c.ivars.is_empty() {
        writeln!(out, "Ivars:")?;
        for i in &c.ivars {
            writeln!(
                out,
                "  {}: {} (offset {}, size {})",
                i.name, i.type_enc, i.offset, i.size,
            )?;
        }
    }
    if !c.properties.is_empty() {
        writeln!(out, "Properties:")?;
        for p in &c.properties {
            writeln!(out, "  {} ({})", p.name, p.attributes)?;
        }
    }
    if !c.adopted_protocols.is_empty() {
        writeln!(out, "Adopted protocols:")?;
        for p in &c.adopted_protocols {
            writeln!(out, "  {p}")?;
        }
    }
    writeln!(out, "@end")
}

fn render_objc_category_detail(
    c: &glass_api::ObjcCategoryDetail,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "@interface {}  // category", c.name)?;
    writeln!(
        out,
        "  // artifact {}  vaddr {}  category_for {}",
        c.artifact, c.vaddr, c.category_for,
    )?;
    if !c.instance_methods.is_empty() {
        writeln!(out, "Instance methods:")?;
        for m in &c.instance_methods {
            write_objc_method(out, '-', &c.name, m)?;
        }
    }
    if !c.class_methods.is_empty() {
        writeln!(out, "Class methods:")?;
        for m in &c.class_methods {
            write_objc_method(out, '+', &c.name, m)?;
        }
    }
    if !c.instance_properties.is_empty() {
        writeln!(out, "Instance properties:")?;
        for p in &c.instance_properties {
            writeln!(out, "  {} ({})", p.name, p.attributes)?;
        }
    }
    if !c.class_properties.is_empty() {
        writeln!(out, "Class properties:")?;
        for p in &c.class_properties {
            writeln!(out, "  {} ({})", p.name, p.attributes)?;
        }
    }
    if !c.protocols.is_empty() {
        writeln!(out, "Protocols:")?;
        for p in &c.protocols {
            writeln!(out, "  {p}")?;
        }
    }
    writeln!(out, "@end")
}

fn write_objc_method(
    out: &mut dyn Write,
    sigil: char,
    class_name: &str,
    m: &glass_api::ObjcMethodEntry,
) -> std::io::Result<()> {
    let imp = m.imp_vaddr.as_deref().unwrap_or("(no imp)");
    writeln!(out, "  {sigil}[{class_name} {}]  @ {imp}  types={}", m.name, m.types)
}

fn render_swift_type_detail(
    kw: &str,
    t: &glass_api::SwiftTypeDetail,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{kw} {}", t.name)?;
    writeln!(
        out,
        "  // artifact {}  descriptor {}",
        t.artifact, t.descriptor_vaddr,
    )?;
    if let Some(p) = &t.parent_vaddr {
        writeln!(out, "  // parent {p}")?;
    }
    if let Some(acc) = &t.metadata_accessor_vaddr {
        writeln!(out, "  // metadata accessor @ {acc}")?;
    }
    if !t.fields.is_empty() {
        writeln!(out, "Fields:")?;
        for f in &t.fields {
            if f.type_pretty.is_empty() {
                writeln!(out, "  {}  (raw type: {:?})", f.name, f.raw_type)?;
            } else {
                writeln!(out, "  {}: {}", f.name, f.type_pretty)?;
            }
        }
    }
    if !t.vtable.is_empty() {
        writeln!(out, "V-table:")?;
        for e in &t.vtable {
            writeln!(out, "  vtable[{}] @ {}  flags 0x{:x}", e.index, e.impl_vaddr, e.flags)?;
        }
    }
    Ok(())
}

pub fn bin_search(
    path: PathBuf,
    artifact: String,
    pattern: String,
    section: Option<String>,
    limit: Option<usize>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.bin_search(&artifact, &pattern, section.as_deref(), limit)
    })?;
    output::emit(envelope, format, render_bin_search)
}

fn render_bin_search(
    data: &glass_api::BinSearchResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}  {} of {} matches for pattern: {}",
        data.artifact, data.shown, data.total, data.pattern,
    )?;
    for m in &data.matches {
        writeln!(
            out,
            "  {:<22}  {:<14}  {}",
            m.section, m.address, m.preview,
        )?;
    }
    Ok(())
}

pub fn insn_search(
    path: PathBuf,
    artifact: String,
    pattern: String,
    section: Option<String>,
    limit: Option<usize>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        let bundle = glass_api::open(&path)?;
        bundle.insn_search(&artifact, &pattern, section.as_deref(), limit)
    })?;
    output::emit(envelope, format, render_insn_search)
}

fn render_insn_search(
    data: &glass_api::InsnSearchResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}  {} of {} matches for pattern: {}  (bytes: {})",
        data.artifact, data.shown, data.total, data.pattern, data.bytes_hex,
    )?;
    for m in &data.matches {
        writeln!(
            out,
            "  {:<22}  {:<14}  {}",
            m.section, m.address, m.preview,
        )?;
    }
    Ok(())
}

// Marker — `Envelope` referenced in the function signatures.
#[allow(dead_code)]
fn _envelope_marker(_: Envelope<()>) {}

// ---- Patch verbs ---------------------------------------------------------

#[derive(serde::Serialize)]
pub struct PatchResult {
    pub patches: PathBuf,
    pub artifact: String,
    pub vaddr: String,
    pub new_bytes_hex: String,
    pub total_edits: usize,
}

pub fn patch(
    path: PathBuf,
    artifact_ref: String,
    addr: String,
    insn: Option<String>,
    bytes: Option<String>,
    patches: PathBuf,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| -> Result<PatchResult> {
        // Resolve the bundle + artifact so we capture the exact
        // 64-char artifact id + grab the bytes-at to record
        // `original_bytes` for the patch entry.
        let bundle = glass_api::open(&path)?;
        let artifact_id = bundle
            .resolve_artifact(&artifact_ref)
            .ok_or_else(|| anyhow::anyhow!("no artifact matches {artifact_ref:?}"))?
            .clone();
        let vaddr = u64::from_str_radix(addr.trim_start_matches("0x"), 16)
            .with_context(|| format!("bad hex address {addr:?}"))?;

        // Encode the new bytes from --insn or --bytes.
        let (new_bytes, kind, source_text) = match (insn, bytes) {
            (Some(insn_src), None) => {
                // Build a symbol lookup so identifiers like `bl
                // decode_packet` resolve. We need the artifact's
                // SymbolMap — built lazily via glass-api's
                // disasm verb, but here we can reach for the
                // container's symbols directly through a one-off
                // build. Cheaper alternative: just rely on
                // armv8-encode's own decoder via a re-open after
                // staging. For v1 of the CLI verb we skip
                // symbol resolution — users can pass hex.
                let bytes_vec = glass_api::compile_insn_at(&insn_src, vaddr, None)?;
                (
                    bytes_vec,
                    glass_api::PatchKind::Instruction,
                    insn_src,
                )
            }
            (None, Some(hex_src)) => {
                let bytes_vec = parse_hex_bytes(&hex_src)?;
                let display = bytes_vec
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                (bytes_vec, glass_api::PatchKind::Bytes, display)
            }
            (Some(_), Some(_)) => anyhow::bail!("provide either --insn or --bytes, not both"),
            (None, None) => anyhow::bail!("provide --insn or --bytes"),
        };

        let new_bytes_hex = new_bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");

        // Load existing patch file (or default), upsert, save.
        let mut pf = glass_api::PatchFile::read_or_default(&patches)?;
        if pf.source_path.is_none() {
            pf.source_path = Some(path.clone());
        }
        let entry = glass_api::PatchEntry {
            artifact: artifact_id.to_hex(),
            vaddr,
            kind,
            new_bytes,
            original_bytes: Vec::new(),
            source_text,
        };
        pf.upsert(entry);
        let total_edits = pf.edits.len();
        pf.write(&patches)?;
        Ok(PatchResult {
            patches,
            artifact: artifact_id.to_hex(),
            vaddr: format!("0x{vaddr:x}"),
            new_bytes_hex,
            total_edits,
        })
    })?;
    output::emit(envelope, format, render_patch)
}

/// Stage a typed smali class rewrite into a patch file. The body
/// is the full class text (everything `glass smali --class …`
/// returns); we parse + validate it against the bundle so the
/// caller gets a fast failure on a malformed body. `body_source`
/// is one of `Inline(text)`, `File(path)`, or `Stdin` —
/// resolved by the CLI layer before calling here.
#[derive(serde::Serialize)]
pub struct SmaliSetResult {
    pub patches: PathBuf,
    pub artifact: String,
    pub class_jni: String,
    pub body_bytes: usize,
    pub total_smali_edits: usize,
}

pub enum SmaliBodySource {
    Inline(String),
    File(PathBuf),
    Stdin,
}

pub fn smali_set(
    path: PathBuf,
    class_ref: String,
    body_source: SmaliBodySource,
    patches: PathBuf,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| -> Result<SmaliSetResult> {
        // Pull the body in first so we fail fast if e.g. the file
        // can't be read — opening the bundle is the expensive step.
        let body = match body_source {
            SmaliBodySource::Inline(s) => s,
            SmaliBodySource::File(p) => std::fs::read_to_string(&p)
                .with_context(|| format!("reading body file {}", p.display()))?,
            SmaliBodySource::Stdin => {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .context("reading body from stdin")?;
                buf
            }
        };
        if body.trim().is_empty() {
            anyhow::bail!("smali body is empty");
        }
        // Parse the body so we catch syntax errors here rather
        // than at export time.
        let parsed = glass_api::parse_smali_class(&body)?;

        let bundle = glass_api::open(&path)?;
        let (artifact_id, class_jni) =
            bundle.resolve_smali_class(&class_ref)?;
        // Make sure the body's `.class` line agrees with what the
        // caller asked to replace — accidental mismatches here
        // would silently overwrite the wrong slot.
        let body_jni = glass_api::smali_class_jni(&parsed);
        if body_jni != class_jni {
            anyhow::bail!(
                "smali body declares class {body_jni:?} but --class resolves to {class_jni:?}"
            );
        }

        let mut pf = glass_api::PatchFile::read_or_default(&patches)?;
        if pf.source_path.is_none() {
            pf.source_path = Some(path.clone());
        }
        let body_bytes = body.len();
        pf.upsert_smali(glass_api::SmaliPatchEntry {
            artifact: artifact_id.to_hex(),
            class_jni: class_jni.clone(),
            body,
        });
        let total_smali_edits = pf.smali_edits.len();
        pf.write(&patches)?;
        Ok(SmaliSetResult {
            patches,
            artifact: artifact_id.to_hex(),
            class_jni,
            body_bytes,
            total_smali_edits,
        })
    })?;
    output::emit(envelope, format, render_smali_set)
}

fn render_smali_set(data: &SmaliSetResult, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        out,
        "staged smali: {} on {} ({} byte{} body) — {} smali edit{} total in {}",
        data.class_jni,
        &data.artifact[..16.min(data.artifact.len())],
        data.body_bytes,
        if data.body_bytes == 1 { "" } else { "s" },
        data.total_smali_edits,
        if data.total_smali_edits == 1 { "" } else { "s" },
        data.patches.display(),
    )
}

fn render_patch(data: &PatchResult, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        out,
        "staged: {} @ {} = {}   ({} edit{} total in {})",
        &data.artifact[..16.min(data.artifact.len())],
        data.vaddr,
        data.new_bytes_hex,
        data.total_edits,
        if data.total_edits == 1 { "" } else { "s" },
        data.patches.display(),
    )
}

#[derive(serde::Serialize)]
pub struct ExportPatchedResult {
    pub out: PathBuf,
    pub edits_applied: usize,
}

pub fn export_patched(
    path: PathBuf,
    patches: PathBuf,
    out: PathBuf,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| -> Result<ExportPatchedResult> {
        let pf = glass_api::PatchFile::read_or_default(&patches)?;
        if pf.edits.is_empty() && pf.smali_edits.is_empty() {
            anyhow::bail!("patch file {} contains no edits", patches.display());
        }
        let edits_applied = pf.edits.len() + pf.smali_edits.len();
        let bundle = glass_api::open(&path)?;
        let edit_map = pf.to_edit_map();
        let smali_map = pf.to_smali_edit_map()?;
        // CLI export doesn't have an "add new file" surface yet
        // — the patch-file format only carries replacements.
        // Pass an empty additions map; the GUI's gadget-injection
        // flow is the only producer of additions today.
        let additions = glass_api::ApkAdditions::new();
        glass_api::export_to_path_with_smali(
            &bundle, &edit_map, &smali_map, &additions, &out,
        )?;
        Ok(ExportPatchedResult {
            out,
            edits_applied,
        })
    })?;
    output::emit(envelope, format, render_export_patched)
}

fn render_export_patched(data: &ExportPatchedResult, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        out,
        "wrote patched bundle to {} ({} edit{} applied)",
        data.out.display(),
        data.edits_applied,
        if data.edits_applied == 1 { "" } else { "s" },
    )
}

pub fn patch_schema(format: Format) -> Result<()> {
    let envelope = output::measured(|| -> Result<serde_json::Value> {
        Ok(glass_api::patch_file_schema())
    })?;
    output::emit(envelope, format, |data, out| {
        let pretty = serde_json::to_string_pretty(data).unwrap_or_default();
        writeln!(out, "{pretty}")
    })
}

// ---- Device verbs --------------------------------------------------

pub fn device_list(format: Format) -> Result<()> {
    let envelope = output::measured(|| -> Result<serde_json::Value> {
        let mgr = glass_device::DeviceManager::new();
        let devices: Vec<serde_json::Value> = mgr
            .list()
            .into_iter()
            .map(|d| {
                serde_json::json!({
                    "platform": d.id.platform.label(),
                    "serial": d.id.serial,
                    "model": d.model,
                    "os_version": d.os_version,
                    "state": format!("{:?}", d.state),
                })
            })
            .collect();
        Ok(serde_json::json!({ "devices": devices }))
    })?;
    output::emit(envelope, format, |data, out| {
        let devs = data.get("devices").and_then(|v| v.as_array());
        match devs {
            Some(d) if !d.is_empty() => {
                for dev in d {
                    let s = |k: &str| -> String {
                        dev.get(k)
                            .and_then(|v| v.as_str())
                            .unwrap_or("-")
                            .to_string()
                    };
                    writeln!(
                        out,
                        "{:<10} {:<20} {:<20} {:<12} {}",
                        s("platform"),
                        s("serial"),
                        s("model"),
                        s("os_version"),
                        s("state"),
                    )?;
                }
                Ok(())
            }
            _ => writeln!(out, "(no devices)"),
        }
    })
}

pub fn device_pidof(serial: String, name: String, format: Format) -> Result<()> {
    let envelope = output::measured(|| -> Result<serde_json::Value> {
        let mgr = glass_device::DeviceManager::new();
        let pids = mgr
            .android_pidof(&serial, &name)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(serde_json::json!({ "pids": pids }))
    })?;
    output::emit(envelope, format, |data, out| {
        let pids = data
            .get("pids")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if pids.is_empty() {
            writeln!(out, "(no matching pids)")
        } else {
            for p in pids {
                writeln!(out, "{p}")?;
            }
            Ok(())
        }
    })
}

pub fn device_launch(serial: String, package: String, format: Format) -> Result<()> {
    let envelope = output::measured(|| -> Result<serde_json::Value> {
        let mgr = glass_device::DeviceManager::new();
        let out = mgr
            .android_launch(&serial, &package)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(serde_json::json!({ "output": out }))
    })?;
    output::emit(envelope, format, |data, out| {
        let s = data.get("output").and_then(|v| v.as_str()).unwrap_or("");
        write!(out, "{s}")
    })
}

pub fn device_force_stop(
    serial: String,
    package: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| -> Result<serde_json::Value> {
        let mgr = glass_device::DeviceManager::new();
        let out = mgr
            .android_force_stop(&serial, &package)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(serde_json::json!({ "output": out }))
    })?;
    output::emit(envelope, format, |data, out| {
        let s = data.get("output").and_then(|v| v.as_str()).unwrap_or("");
        write!(out, "{s}")
    })
}

pub fn device_pull(
    serial: String,
    remote: String,
    local: PathBuf,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| -> Result<serde_json::Value> {
        let mgr = glass_device::DeviceManager::new();
        let out = mgr
            .android_pull(&serial, &remote, &local)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(serde_json::json!({
            "output": out,
            "local": local.display().to_string(),
        }))
    })?;
    output::emit(envelope, format, |data, out| {
        let s = data.get("output").and_then(|v| v.as_str()).unwrap_or("");
        write!(out, "{s}")
    })
}

pub fn device_push(
    serial: String,
    local: PathBuf,
    remote: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| -> Result<serde_json::Value> {
        let mgr = glass_device::DeviceManager::new();
        let out = mgr
            .android_push(&serial, &local, &remote)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(serde_json::json!({
            "output": out,
            "remote": remote,
        }))
    })?;
    output::emit(envelope, format, |data, out| {
        let s = data.get("output").and_then(|v| v.as_str()).unwrap_or("");
        write!(out, "{s}")
    })
}

pub fn device_shell(
    serial: String,
    args: Vec<String>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| -> Result<serde_json::Value> {
        let mgr = glass_device::DeviceManager::new();
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = mgr
            .android_shell(&serial, &refs)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(serde_json::json!({ "stdout": out }))
    })?;
    output::emit(envelope, format, |data, out| {
        let s = data.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
        write!(out, "{s}")
    })
}

/// Parse a hex byte string like `"20 00 80 52"` (whitespace
/// optional) into a Vec<u8>. Used by the `patch --bytes` path.
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.len() % 2 != 0 {
        anyhow::bail!("hex byte string has odd length: {s:?}");
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for i in (0..cleaned.len()).step_by(2) {
        let pair = &cleaned[i..i + 2];
        let byte = u8::from_str_radix(pair, 16)
            .with_context(|| format!("non-hex pair {pair:?}"))?;
        out.push(byte);
    }
    Ok(out)
}
