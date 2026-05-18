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

