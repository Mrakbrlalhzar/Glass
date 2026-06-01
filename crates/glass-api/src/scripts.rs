//! Frida script library + per-bundle enable state.
//!
//! ## Storage
//!
//! Two halves on purpose:
//!
//! - **Body**: plain `.js` files under `glass_db::scripts_dir()`
//!   (e.g. `~/Library/Application Support/Glass/scripts/`). Flat
//!   layout — `<name>.js`. Edit them in your normal editor if you
//!   like; we re-read on every call, no caching.
//! - **Metadata** (description, tags, timestamps) and **per-bundle
//!   enabled flags**: redb tables, surfaced via `Database`. The
//!   metadata is global; enabled-ness is keyed by bundle id so the
//!   same script can be on for one app and off for another.
//!
//! ## Verb surface
//!
//! - `scripts()` — enumerate the library; merges files + metadata.
//! - `read_script(name)` — return the body.
//! - `write_script(name, body, description?, tags?)` — upsert.
//! - `delete_script(name)` — remove file + meta + every enabled row.
//! - `set_script_enabled(bundle_path, name, enabled)` — toggle for
//!   one bundle.
//! - `enabled_scripts(bundle_path)` — list enabled names.
//!
//! Reconciliation rules:
//! - A file on disk with no metadata row is still listed (as a
//!   plain entry with empty description / tags). The GUI's editor
//!   creates a metadata row on first save.
//! - A metadata row whose `.js` file is missing is reported as
//!   `present_on_disk = false` so the picker can surface "stale"
//!   entries without dropping them silently.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use glass_db::Database;
use serde::Serialize;

/// One script in the library — body excluded (call `read_script`
/// to fetch it). `enabled_for_bundle` is filled in by the
/// `scripts_for_bundle` variant; the bundle-agnostic `scripts`
/// verb leaves it as `false` regardless of state.
#[derive(Serialize, Debug, Clone)]
pub struct ScriptInfo {
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    /// Bytes of the `.js` file. `None` when the file is missing
    /// (orphan metadata row).
    pub size_bytes: Option<u64>,
    pub present_on_disk: bool,
    pub created_unix: u64,
    pub modified_unix: u64,
    /// Only meaningful when called via `scripts_for_bundle`; the
    /// bundle-agnostic listing keeps this `false`.
    #[serde(default)]
    pub enabled_for_bundle: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct ScriptsResult {
    pub directory: String,
    pub total: usize,
    pub scripts: Vec<ScriptInfo>,
    /// Only set when this listing was scoped to a bundle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ScriptReadResult {
    pub name: String,
    pub body: String,
    pub size_bytes: u64,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ScriptWriteResult {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub created: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct ScriptDeleteResult {
    pub name: String,
    pub removed_file: bool,
    pub removed_meta: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct ScriptEnableResult {
    pub bundle_id: String,
    pub name: String,
    pub enabled: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct EnabledScriptsResult {
    pub bundle_id: String,
    pub names: Vec<String>,
}

/// List every known script. Bundle-agnostic; `enabled_for_bundle`
/// is always false in the result.
pub fn scripts() -> Result<ScriptsResult> {
    list_inner(None)
}

/// Same, scoped to a specific bundle (so each entry's
/// `enabled_for_bundle` reflects redb state).
pub fn scripts_for_bundle(bundle_path: impl AsRef<Path>) -> Result<ScriptsResult> {
    let path = bundle_path.as_ref();
    let bid = bundle_id_from_path(path)?;
    list_inner(Some(bid))
}

fn list_inner(scope: Option<glass_db::BundleId>) -> Result<ScriptsResult> {
    let dir = glass_db::scripts_dir().context("resolving scripts dir")?;
    let db = Database::open(false).context("opening glass-db (read-only)")?;
    let meta_map = db.all_script_meta();
    let enabled_set: std::collections::HashSet<String> = match scope.as_ref() {
        Some(bid) => db.enabled_scripts(bid).into_iter().collect(),
        None => std::collections::HashSet::new(),
    };
    let mut entries: std::collections::BTreeMap<String, ScriptInfo> =
        std::collections::BTreeMap::new();

    // Pass 1: walk the directory. Each `.js` file becomes an
    // entry; merge with redb metadata if any.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = script_name_from_path(&path) else { continue };
            let size = path.metadata().ok().map(|m| m.len());
            let meta = meta_map.get(&name).cloned().unwrap_or_default();
            let enabled = enabled_set.contains(&name);
            entries.insert(
                name.clone(),
                ScriptInfo {
                    name,
                    description: meta.description,
                    tags: meta.tags,
                    size_bytes: size,
                    present_on_disk: true,
                    created_unix: meta.created_unix,
                    modified_unix: meta.modified_unix,
                    enabled_for_bundle: enabled,
                },
            );
        }
    }
    // Pass 2: surface orphan metadata rows so the user can clean
    // them up (delete-script gets rid of both halves).
    for (name, meta) in meta_map {
        entries.entry(name.clone()).or_insert(ScriptInfo {
            name: name.clone(),
            description: meta.description,
            tags: meta.tags,
            size_bytes: None,
            present_on_disk: false,
            created_unix: meta.created_unix,
            modified_unix: meta.modified_unix,
            enabled_for_bundle: enabled_set.contains(&name),
        });
    }

    let scripts: Vec<ScriptInfo> = entries.into_values().collect();
    Ok(ScriptsResult {
        directory: dir.display().to_string(),
        total: scripts.len(),
        scripts,
        bundle_id: scope.map(|b| b.to_string()),
    })
}

pub fn read_script(name: &str) -> Result<ScriptReadResult> {
    let name = sanitize_name(name)?;
    let path = script_path(&name)?;
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let db = Database::open(false).context("opening glass-db (read-only)")?;
    let meta = db.script_meta(&name).unwrap_or_default();
    Ok(ScriptReadResult {
        name,
        size_bytes: body.len() as u64,
        body,
        description: meta.description,
        tags: meta.tags,
    })
}

/// Create or overwrite `<name>.js` and refresh metadata. Pass
/// `None` for description / tags to leave the existing values
/// alone; pass `Some("")` / `Some(vec![])` to clear them.
pub fn write_script(
    name: &str,
    body: &str,
    description: Option<&str>,
    tags: Option<Vec<String>>,
) -> Result<ScriptWriteResult> {
    let name = sanitize_name(name)?;
    let dir = glass_db::scripts_dir().context("resolving scripts dir")?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{name}.js"));
    let created = !path.exists();
    std::fs::write(&path, body)
        .with_context(|| format!("writing {}", path.display()))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let db = Database::open(false).context("opening glass-db")?;
    let mut meta = db.script_meta(&name).unwrap_or_default();
    if meta.created_unix == 0 {
        meta.created_unix = now;
    }
    meta.modified_unix = now;
    if let Some(d) = description {
        meta.description = d.to_string();
    }
    if let Some(t) = tags {
        meta.tags = t;
    }
    db.save_script_meta(&name, &meta)?;

    Ok(ScriptWriteResult {
        name,
        path: path.display().to_string(),
        size_bytes: body.len() as u64,
        created,
    })
}

pub fn delete_script(name: &str) -> Result<ScriptDeleteResult> {
    let name = sanitize_name(name)?;
    let dir = glass_db::scripts_dir().context("resolving scripts dir")?;
    let path = dir.join(format!("{name}.js"));
    let removed_file = match std::fs::remove_file(&path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(anyhow!(
                "removing {}: {e}",
                path.display(),
            ));
        }
    };
    let db = Database::open(false).context("opening glass-db")?;
    let had_meta = db.script_meta(&name).is_some();
    db.delete_script(&name)?;
    Ok(ScriptDeleteResult {
        name,
        removed_file,
        removed_meta: had_meta,
    })
}

pub fn set_script_enabled(
    bundle_path: impl AsRef<Path>,
    name: &str,
    enabled: bool,
) -> Result<ScriptEnableResult> {
    let name = sanitize_name(name)?;
    let path = bundle_path.as_ref();
    let bid = bundle_id_from_path(path)?;
    let db = Database::open(false).context("opening glass-db")?;
    db.set_script_enabled(&bid, &name, enabled)?;
    Ok(ScriptEnableResult {
        bundle_id: bid.to_string(),
        name,
        enabled,
    })
}

pub fn enabled_scripts(
    bundle_path: impl AsRef<Path>,
) -> Result<EnabledScriptsResult> {
    let path = bundle_path.as_ref();
    let bid = bundle_id_from_path(path)?;
    let db = Database::open(false).context("opening glass-db (read-only)")?;
    Ok(EnabledScriptsResult {
        bundle_id: bid.to_string(),
        names: db.enabled_scripts(&bid),
    })
}

// ---- helpers -----------------------------------------------------------------

/// Names go straight into the filesystem, so reject anything
/// with separators, dot-prefixes, or non-printable characters.
/// Trim the optional `.js` so callers can pass either form.
fn sanitize_name(raw: &str) -> Result<String> {
    let trimmed = raw.strip_suffix(".js").unwrap_or(raw).trim();
    if trimmed.is_empty() {
        return Err(anyhow!("script name is empty"));
    }
    if trimmed.starts_with('.') {
        return Err(anyhow!(
            "script name {trimmed:?} starts with '.' (reserved)",
        ));
    }
    for c in trimmed.chars() {
        if c == '/' || c == '\\' || c == ':' || c.is_control() {
            return Err(anyhow!(
                "script name {trimmed:?} contains invalid char {c:?}",
            ));
        }
    }
    Ok(trimmed.to_string())
}

fn script_path(name: &str) -> Result<PathBuf> {
    let dir = glass_db::scripts_dir().context("resolving scripts dir")?;
    Ok(dir.join(format!("{name}.js")))
}

fn script_name_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let ext = path.extension()?.to_str()?;
    if ext != "js" {
        return None;
    }
    if stem.starts_with('.') {
        return None;
    }
    Some(stem.to_string())
}

/// Content-hash a bundle path the same way `glass open` does, so
/// per-bundle keys round-trip with the GUI's state.
fn bundle_id_from_path(path: &Path) -> Result<glass_db::BundleId> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(glass_db::BundleId::from_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_path_traversal() {
        assert!(sanitize_name("../etc/passwd").is_err());
        assert!(sanitize_name("foo/bar").is_err());
        assert!(sanitize_name(".hidden").is_err());
        assert!(sanitize_name("").is_err());
        assert!(sanitize_name("   ").is_err());
    }

    #[test]
    fn sanitize_strips_extension_and_keeps_dashes() {
        assert_eq!(sanitize_name("anti-root").unwrap(), "anti-root");
        assert_eq!(sanitize_name("anti-root.js").unwrap(), "anti-root");
        assert_eq!(sanitize_name(" log_crypto ").unwrap(), "log_crypto");
    }

    #[test]
    fn sanitize_rejects_control_chars() {
        assert!(sanitize_name("foo\nbar").is_err());
        assert!(sanitize_name("foo\tbar").is_err());
    }
}
