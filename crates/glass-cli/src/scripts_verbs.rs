//! CLI verbs for the Frida script library.
//!
//! Sibling to `verbs.rs`. Kept separate because most of these
//! don't take a bundle `path` (the script library is global) —
//! mixing them into the path-first `verbs.rs` made the dispatch
//! arms harder to read. The text renderers stay terse: scripts
//! are a small list, the JSON form is the authoritative output.

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;

use crate::output::{self, Format};

pub fn scripts(bundle_path: Option<PathBuf>, format: Format) -> Result<()> {
    let envelope = output::measured(|| match bundle_path {
        Some(p) => glass_api::scripts_for_bundle(p),
        None => glass_api::scripts(),
    })?;
    output::emit(envelope, format, render_scripts)
}

pub fn script_read(name: String, format: Format) -> Result<()> {
    let envelope = output::measured(|| glass_api::read_script(&name))?;
    output::emit(envelope, format, render_script_read)
}

pub fn script_write(
    name: String,
    body: Option<String>,
    body_file: Option<PathBuf>,
    description: Option<String>,
    tags: Option<Vec<String>>,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| -> Result<glass_api::ScriptWriteResult> {
        let body = match (body, body_file) {
            (Some(_), Some(_)) => {
                anyhow::bail!("pass --body or --body-file, not both")
            }
            (Some(s), None) => s,
            (None, Some(p)) => std::fs::read_to_string(&p)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?,
            (None, None) => {
                anyhow::bail!("expected --body <text> or --body-file <path>")
            }
        };
        glass_api::write_script(
            &name,
            &body,
            description.as_deref(),
            tags,
        )
    })?;
    output::emit(envelope, format, render_script_write)
}

pub fn script_delete(name: String, format: Format) -> Result<()> {
    let envelope = output::measured(|| glass_api::delete_script(&name))?;
    output::emit(envelope, format, render_script_delete)
}

pub fn script_enable(
    bundle_path: PathBuf,
    name: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        glass_api::set_script_enabled(&bundle_path, &name, true)
    })?;
    output::emit(envelope, format, render_script_enable)
}

pub fn script_disable(
    bundle_path: PathBuf,
    name: String,
    format: Format,
) -> Result<()> {
    let envelope = output::measured(|| {
        glass_api::set_script_enabled(&bundle_path, &name, false)
    })?;
    output::emit(envelope, format, render_script_enable)
}

pub fn enabled_scripts(bundle_path: PathBuf, format: Format) -> Result<()> {
    let envelope = output::measured(|| glass_api::enabled_scripts(&bundle_path))?;
    output::emit(envelope, format, render_enabled_scripts)
}

// ---- text renderers ---------------------------------------------------------

fn render_scripts(
    data: &glass_api::ScriptsResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{} scripts in {}", data.total, data.directory)?;
    if let Some(bid) = &data.bundle_id {
        writeln!(out, "(scoped to bundle {bid})")?;
    }
    for s in &data.scripts {
        let on_disk = if s.present_on_disk { "" } else { " [missing]" };
        let enabled = if s.enabled_for_bundle { "*" } else { " " };
        let size = s
            .size_bytes
            .map(|n| format!("{n}B"))
            .unwrap_or_else(|| "?".to_string());
        writeln!(out, "  {enabled} {} ({size}){on_disk}", s.name)?;
        if !s.description.is_empty() {
            writeln!(out, "        {}", s.description)?;
        }
        if !s.tags.is_empty() {
            writeln!(out, "        tags: {}", s.tags.join(", "))?;
        }
    }
    Ok(())
}

fn render_script_read(
    data: &glass_api::ScriptReadResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "// {} ({} bytes)", data.name, data.size_bytes)?;
    if !data.description.is_empty() {
        writeln!(out, "// {}", data.description)?;
    }
    out.write_all(data.body.as_bytes())?;
    if !data.body.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

fn render_script_write(
    data: &glass_api::ScriptWriteResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let verb = if data.created { "created" } else { "updated" };
    writeln!(
        out,
        "{verb} {} ({} bytes) -> {}",
        data.name, data.size_bytes, data.path,
    )
}

fn render_script_delete(
    data: &glass_api::ScriptDeleteResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}: file={}, meta={}",
        data.name,
        if data.removed_file { "removed" } else { "absent" },
        if data.removed_meta { "removed" } else { "absent" },
    )
}

fn render_script_enable(
    data: &glass_api::ScriptEnableResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{} for bundle {}: {}",
        data.name,
        data.bundle_id,
        if data.enabled { "enabled" } else { "disabled" },
    )
}

fn render_enabled_scripts(
    data: &glass_api::EnabledScriptsResult,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "{} enabled scripts for {}", data.names.len(), data.bundle_id)?;
    for n in &data.names {
        writeln!(out, "  {n}")?;
    }
    Ok(())
}
