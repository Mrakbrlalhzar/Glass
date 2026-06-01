//! Tool-name → `glass_api` dispatch.
//!
//! One arm per verb. Each arm pulls typed args out of the JSON
//! object the MCP client sent, calls the matching `glass_api`
//! function, and serialises the result back to a JSON string for
//! the `text` content block.
//!
//! Every arm goes through the same envelope as the CLI:
//! `{ "data": ..., "meta": { "duration_ms": ... } }`. Keeping the
//! shape consistent between CLI and MCP means a single piece of
//! downstream tooling (jq filter, schema, prompt example) can
//! consume either source.

use std::path::PathBuf;
use std::time::Instant;

use serde_json::{json, Value};

use crate::DispatchError;

type Result<T> = std::result::Result<T, DispatchError>;

pub(crate) fn call(name: &str, args: &Value) -> Result<String> {
    let start = Instant::now();
    let data: Value = match name {
        "inspect" => {
            let bundle = open(args)?;
            json_of(&bundle.inspect())?
        }
        "artifacts" => {
            let bundle = open(args)?;
            json_of(&bundle.artifacts())?
        }
        "sections" => {
            let bundle = open(args)?;
            let artifact = opt_str(args, "artifact");
            json_of(&bundle.sections(artifact.as_deref()))?
        }
        "binary-info" => {
            let bundle = open(args)?;
            json_of(&bundle.binary_info())?
        }
        "hash" => {
            let path = require_path(args)?;
            json_of(&glass_api::hash_file(&path)?)?
        }
        "symbols" => {
            let bundle = open(args)?;
            let artifact = opt_str(args, "artifact");
            let filter = opt_str(args, "filter");
            let kind = opt_str(args, "kind").as_deref().and_then(parse_kind);
            let limit = opt_usize(args, "limit");
            let query = glass_api::SymbolQuery {
                artifact: artifact.as_deref(),
                filter: filter.as_deref(),
                kind,
                limit,
            };
            json_of(&bundle.symbols(query))?
        }
        "symbol-at" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let addr = require_hex_u64(args, "addr")?;
            json_of(&bundle.symbol_at(&artifact, addr))?
        }
        "demangle" => {
            let name = require_str(args, "name")?;
            json_of(&glass_api::demangle(&name))?
        }
        "disasm" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let section = opt_str(args, "section");
            let limit = opt_usize(args, "limit");
            json_of(&bundle.disasm(&artifact, section.as_deref(), limit)?)?
        }
        "decode" => {
            let word_s = require_str(args, "word")?;
            let word = u32::from_str_radix(word_s.trim_start_matches("0x"), 16)
                .map_err(|e| DispatchError::Other(format!("bad word {word_s:?}: {e}")))?;
            let addr = match opt_str(args, "addr") {
                Some(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16)
                    .map_err(|e| DispatchError::Other(format!("bad addr {s:?}: {e}")))?,
                None => 0,
            };
            json_of(&glass_api::decode_word(word, addr))?
        }
        "cfg-of" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let func = require_str(args, "func")?;
            json_of(&bundle.cfg(&artifact, &func)?)?
        }
        "calls-from" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let func = require_str(args, "func")?;
            json_of(&bundle.calls_from(&artifact, &func)?)?
        }
        "classes" => {
            let bundle = open(args)?;
            let package = opt_str(args, "package");
            json_of(&bundle.classes(package.as_deref()))?
        }
        "types" => {
            let bundle = open(args)?;
            let artifact = opt_str(args, "artifact");
            let package = opt_str(args, "package");
            let kind = match opt_str(args, "kind") {
                Some(s) => match glass_api::TypeKind::parse(&s) {
                    Some(k) => Some(k),
                    None => {
                        return Err(DispatchError::Other(format!(
                            "unknown kind {s:?}: expected objc-class, objc-category, swift-class, swift-struct, swift-enum"
                        )))
                    }
                },
                None => None,
            };
            let limit = opt_usize(args, "limit").or(Some(200));
            json_of(&bundle.types(
                artifact.as_deref(),
                kind,
                package.as_deref(),
                limit,
            )?)?
        }
        "type" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let name = require_str(args, "name")?;
            let raw = args
                .get("raw")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            json_of(&bundle.type_detail(&artifact, &name, raw)?)?
        }
        "scripts" => {
            // Optional `path` scopes to a bundle so each row carries
            // `enabled_for_bundle`. Without it the listing is global.
            match opt_str(args, "path") {
                Some(p) => json_of(&glass_api::scripts_for_bundle(&p)?)?,
                None => json_of(&glass_api::scripts()?)?,
            }
        }
        "script-read" => {
            let name = require_str(args, "name")?;
            json_of(&glass_api::read_script(&name)?)?
        }
        "script-write" => {
            let name = require_str(args, "name")?;
            let body = require_str(args, "body")?;
            let description = opt_str(args, "description");
            let tags = args
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                });
            json_of(&glass_api::write_script(
                &name,
                &body,
                description.as_deref(),
                tags,
            )?)?
        }
        "script-delete" => {
            let name = require_str(args, "name")?;
            json_of(&glass_api::delete_script(&name)?)?
        }
        "script-enable" => {
            let path = require_path(args)?;
            let name = require_str(args, "name")?;
            json_of(&glass_api::set_script_enabled(&path, &name, true)?)?
        }
        "script-disable" => {
            let path = require_path(args)?;
            let name = require_str(args, "name")?;
            json_of(&glass_api::set_script_enabled(&path, &name, false)?)?
        }
        "enabled-scripts" => {
            let path = require_path(args)?;
            json_of(&glass_api::enabled_scripts(&path)?)?
        }
        "smali" => {
            let bundle = open(args)?;
            let class = require_str(args, "class")?;
            json_of(&bundle.smali(&class)?)?
        }
        "methods" => {
            let bundle = open(args)?;
            let class = require_str(args, "class")?;
            json_of(&bundle.methods(&class)?)?
        }
        "fields" => {
            let bundle = open(args)?;
            let class = require_str(args, "class")?;
            json_of(&bundle.fields(&class)?)?
        }
        "method-calls" => {
            let bundle = open(args)?;
            let class = require_str(args, "class")?;
            let method = require_str(args, "method")?;
            json_of(&bundle.method_calls(&class, &method)?)?
        }
        "xref-addr" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let addr = require_hex_u64(args, "addr")?;
            json_of(&bundle.xref_addr(&artifact, addr)?)?
        }
        "callers" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let symbol = require_str(args, "symbol")?;
            json_of(&bundle.callers(&artifact, &symbol)?)?
        }
        "dex-callers" => {
            let bundle = open(args)?;
            let method = require_str(args, "method")?;
            json_of(&bundle.dex_callers(&method))?
        }
        "field-refs" => {
            let bundle = open(args)?;
            let field = require_str(args, "field")?;
            json_of(&bundle.field_refs(&field))?
        }
        "search" => {
            let bundle = open(args)?;
            let query = require_str(args, "query")?;
            let limit = opt_usize(args, "limit");
            json_of(&bundle.search(&query, limit))?
        }
        "strings" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let min = opt_usize(args, "min");
            let limit = opt_usize(args, "limit");
            json_of(&bundle.strings(&artifact, min, limit)?)?
        }
        "annotations" => {
            let path = require_path(args)?;
            json_of(&glass_api::annotations(&path)?)?
        }
        "db-dump" => {
            let path = require_path(args)?;
            json_of(&glass_api::db_dump(&path)?)?
        }
        "set-rename" => {
            let path = require_path(args)?;
            let key_kind = require_str(args, "key_kind")?;
            let key = require_str(args, "key")?;
            let method = opt_str(args, "method");
            let name = require_str(args, "name")?;
            let key_args = glass_api::AnnotationKeyArgs {
                kind: &key_kind,
                key: &key,
                method: method.as_deref(),
            };
            json_of(&glass_api::set_rename(&path, key_args, &name)?)?
        }
        "set-comment" => {
            let path = require_path(args)?;
            let key_kind = require_str(args, "key_kind")?;
            let key = require_str(args, "key")?;
            let method = opt_str(args, "method");
            let body = require_str(args, "body")?;
            let key_args = glass_api::AnnotationKeyArgs {
                kind: &key_kind,
                key: &key,
                method: method.as_deref(),
            };
            json_of(&glass_api::set_comment(&path, key_args, &body)?)?
        }
        "set-colour" => {
            let path = require_path(args)?;
            let key_kind = require_str(args, "key_kind")?;
            let key = require_str(args, "key")?;
            let method = opt_str(args, "method");
            let rgba = require_str(args, "rgba")?;
            let key_args = glass_api::AnnotationKeyArgs {
                kind: &key_kind,
                key: &key,
                method: method.as_deref(),
            };
            json_of(&glass_api::set_colour(&path, key_args, &rgba)?)?
        }
        "clear-annotation" => {
            let path = require_path(args)?;
            let key_kind = require_str(args, "key_kind")?;
            let key = require_str(args, "key")?;
            let method = opt_str(args, "method");
            let key_args = glass_api::AnnotationKeyArgs {
                kind: &key_kind,
                key: &key,
                method: method.as_deref(),
            };
            json_of(&glass_api::clear_annotation(&path, key_args)?)?
        }
        "bin-search" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let pattern = require_str(args, "pattern")?;
            let section = opt_str(args, "section");
            let limit = opt_usize(args, "limit");
            json_of(&bundle.bin_search(&artifact, &pattern, section.as_deref(), limit)?)?
        }
        "insn-search" => {
            let bundle = open(args)?;
            let artifact = require_str(args, "artifact")?;
            let pattern = require_str(args, "pattern")?;
            let section = opt_str(args, "section");
            let limit = opt_usize(args, "limit");
            json_of(&bundle.insn_search(&artifact, &pattern, section.as_deref(), limit)?)?
        }
        "patch" => {
            // Reuses the same code path as the CLI verb by
            // calling glass_api::PatchFile + compile_insn_at.
            let path = require_path(args)?;
            let artifact_ref = require_str(args, "artifact")?;
            let addr_str = require_str(args, "addr")?;
            let patches_path = std::path::PathBuf::from(require_str(args, "patches")?);
            let insn = opt_str(args, "insn");
            let bytes = opt_str(args, "bytes");

            let bundle = glass_api::open(&path)?;
            let artifact_id = bundle
                .resolve_artifact(&artifact_ref)
                .ok_or_else(|| {
                    DispatchError::Other(format!(
                        "no artifact matches {artifact_ref:?}"
                    ))
                })?
                .clone();
            let vaddr = u64::from_str_radix(addr_str.trim_start_matches("0x"), 16)
                .map_err(|e| {
                    DispatchError::Other(format!(
                        "bad hex address {addr_str:?}: {e}"
                    ))
                })?;
            let (new_bytes, kind, source_text) = match (insn, bytes) {
                (Some(insn_src), None) => {
                    let bytes_vec = glass_api::compile_insn_at(&insn_src, vaddr, None)?;
                    (bytes_vec, glass_api::PatchKind::Instruction, insn_src)
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
                (Some(_), Some(_)) => {
                    return Err(DispatchError::Other("provide either `insn` or `bytes`, not both".to_string()))
                }
                (None, None) => {
                    return Err(DispatchError::Other("provide `insn` or `bytes`".to_string()))
                }
            };
            let mut pf = glass_api::PatchFile::read_or_default(&patches_path)?;
            if pf.source_path.is_none() {
                pf.source_path = Some(path.clone());
            }
            pf.upsert(glass_api::PatchEntry {
                artifact: artifact_id.to_hex(),
                vaddr,
                kind,
                new_bytes: new_bytes.clone(),
                original_bytes: Vec::new(),
                source_text,
            });
            pf.write(&patches_path)?;
            let new_bytes_hex = new_bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            json_of(&serde_json::json!({
                "patches": patches_path,
                "artifact": artifact_id.to_hex(),
                "vaddr": format!("0x{vaddr:x}"),
                "new_bytes_hex": new_bytes_hex,
                "total_edits": pf.edits.len(),
            }))?
        }
        "smali-set" => {
            let path = require_path(args)?;
            let class_ref = require_str(args, "class")?;
            let body = require_str(args, "body")?;
            let patches_path = std::path::PathBuf::from(require_str(args, "patches")?);
            if body.trim().is_empty() {
                return Err(DispatchError::Other("smali body is empty".to_string()));
            }
            let parsed = glass_api::parse_smali_class(&body)?;
            let bundle = glass_api::open(&path)?;
            let (artifact_id, class_jni) =
                bundle.resolve_smali_class(&class_ref)?;
            let body_jni = glass_api::smali_class_jni(&parsed);
            if body_jni != class_jni {
                return Err(DispatchError::Other(format!(
                    "smali body declares class {body_jni:?} but `class` resolves to {class_jni:?}"
                )));
            }
            let mut pf = glass_api::PatchFile::read_or_default(&patches_path)?;
            if pf.source_path.is_none() {
                pf.source_path = Some(path.clone());
            }
            let body_bytes = body.len();
            pf.upsert_smali(glass_api::SmaliPatchEntry {
                artifact: artifact_id.to_hex(),
                class_jni: class_jni.clone(),
                body,
            });
            pf.write(&patches_path)?;
            json_of(&serde_json::json!({
                "patches": patches_path,
                "artifact": artifact_id.to_hex(),
                "class_jni": class_jni,
                "body_bytes": body_bytes,
                "total_smali_edits": pf.smali_edits.len(),
            }))?
        }
        "export-patched" => {
            let path = require_path(args)?;
            let patches = std::path::PathBuf::from(require_str(args, "patches")?);
            let out = std::path::PathBuf::from(require_str(args, "out")?);
            let pf = glass_api::PatchFile::read_or_default(&patches)?;
            if pf.edits.is_empty() && pf.smali_edits.is_empty() {
                return Err(DispatchError::Other(format!(
                    "patch file {} contains no edits",
                    patches.display()
                )));
            }
            let edits_applied = pf.edits.len() + pf.smali_edits.len();
            let bundle = glass_api::open(&path)?;
            let edit_map = pf.to_edit_map();
            let smali_map = pf.to_smali_edit_map()?;
            // MCP export carries no additions today — same shape
            // as the CLI verb; the gadget-injection flow lives
            // in the GUI.
            let additions = glass_api::ApkAdditions::new();
            glass_api::export_to_path_with_smali(
                &bundle, &edit_map, &smali_map, &additions, &out,
            )?;
            json_of(&serde_json::json!({
                "out": out,
                "edits_applied": edits_applied,
            }))?
        }
        "patch-schema" => json_of(&glass_api::patch_file_schema())?,
        other => return Err(DispatchError::UnknownTool(other.to_string())),
    };
    let duration_ms = start.elapsed().as_millis();
    let envelope = json!({ "data": data, "meta": { "duration_ms": duration_ms } });
    serde_json::to_string(&envelope).map_err(DispatchError::from)
}

// ---- argument helpers ----------------------------------------------------

fn open(args: &Value) -> Result<glass_api::Bundle> {
    let path = require_path(args)?;
    Ok(glass_api::open(path)?)
}

fn require_path(args: &Value) -> Result<PathBuf> {
    Ok(PathBuf::from(require_str(args, "path")?))
}

fn require_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| DispatchError::Other(format!("missing required string arg {key:?}")))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

fn require_hex_u64(args: &Value, key: &str) -> Result<u64> {
    let s = require_str(args, key)?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| DispatchError::Other(format!("bad hex {key} {s:?}: {e}")))
}

fn json_of<T: serde::Serialize>(v: &T) -> Result<Value> {
    Ok(serde_json::to_value(v)?)
}

fn parse_kind(s: &str) -> Option<glass_api::SymbolKindName> {
    match s {
        "function" => Some(glass_api::SymbolKindName::Function),
        "object" => Some(glass_api::SymbolKindName::Object),
        "other" => Some(glass_api::SymbolKindName::Other),
        _ => None,
    }
}


/// Parse a hex byte string like `"20 00 80 52"` (whitespace
/// optional) into a Vec<u8>. Mirrors the CLI's helper.
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.len() % 2 != 0 {
        return Err(DispatchError::Other(format!(
            "hex byte string has odd length: {s:?}"
        )));
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for i in (0..cleaned.len()).step_by(2) {
        let pair = &cleaned[i..i + 2];
        let byte = u8::from_str_radix(pair, 16).map_err(|e| {
            DispatchError::Other(format!("non-hex pair {pair:?}: {e}"))
        })?;
        out.push(byte);
    }
    Ok(out)
}
