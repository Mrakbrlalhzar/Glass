//! Skill catalog — the machine-readable index of every automation
//! verb. Consumed by the `glass skills` subcommand (prints as JSON)
//! and by `glass-mcp` (registers each entry as an MCP tool).
//!
//! Schemas are hand-written rather than derived because the
//! descriptions are tuned for LLM tool-use: every field carries a
//! one-line hint about how to populate it, with concrete examples
//! pulled from the Glass workflow. Auto-generated schemas from
//! clap don't capture that nuance.

use serde::Serialize;
use serde_json::{json, Value};

#[derive(Serialize, Debug, Clone)]
pub struct SkillCatalog {
    pub version: &'static str,
    pub skills: Vec<Skill>,
}

#[derive(Serialize, Debug, Clone)]
pub struct Skill {
    /// kebab-case CLI subcommand name, used as the MCP tool name.
    pub name: &'static str,
    /// One-paragraph description rendered to LLMs at tool-listing time.
    pub description: &'static str,
    /// JSON Schema (draft 2020-12 subset) describing the args object.
    pub input_schema: Value,
    /// JSON Schema for the `data` field of the response. May be a
    /// loose hint — consumers should treat the actual JSON as
    /// authoritative.
    pub output_shape: Value,
    /// One-line example invocation, CLI form. Helps an LLM
    /// understand argument flow.
    pub example: &'static str,
}

/// Path argument — used by almost every verb.
fn path_arg() -> Value {
    json!({
        "type": "string",
        "description": "Filesystem path to the bundle or binary (.apk, .aab, .ipa, .so, .dylib, raw executable)."
    })
}

/// Artifact reference — exact label or hex prefix of the
/// content-hash id.
fn artifact_arg() -> Value {
    json!({
        "type": "string",
        "description": "Artifact label (e.g. 'arm64-v8a/libfoo.so', 'glass', the framework name) or any hex prefix of its content-hash id. Use the 'artifacts' verb to enumerate."
    })
}

fn class_arg() -> Value {
    json!({
        "type": "string",
        "description": "DEX class — JNI form ('Lcom/example/Foo;') or Java form ('com.example.Foo')."
    })
}

fn hex_addr_arg() -> Value {
    json!({
        "type": "string",
        "description": "AArch64 virtual address as hex, with or without 0x prefix (e.g. '0x1000058d4' or '1000058d4')."
    })
}

/// Shared `properties` object used by every annotation write verb.
/// `key_kind` selects which AnnotationKey variant is built; `key`
/// is the kind-specific identifier; `method` is required only when
/// `key_kind == "method"`.
fn annotation_key_props() -> Value {
    json!({
        "key_kind": {
            "type": "string",
            "enum": ["address", "symbol", "class", "method", "method-line"],
            "description": "Which kind of thing the annotation hangs off. `address` = native VA; `symbol` = native symbol name; `class` = DEX class JNI; `method` = whole DEX method (pair with `method`); `method-line` = a specific line within a DEX method body (pair `method` with 'name(descriptor)return#<line_offset>')."
        },
        "key": {
            "type": "string",
            "description": "For `address`: hex VA (0x...). For `symbol`: display name or raw name. For `class`, `method`, `method-line`: class JNI (Lcom/example/Foo;)."
        },
        "method": {
            "type": "string",
            "description": "For `method`: 'name(descriptor)return' (e.g. 'bar(Ljava/lang/String;)V'). For `method-line`: same form with '#<line_offset>' appended (e.g. 'bar(Ljava/lang/String;)V#7' for line 7 inside the method body; 0 targets the .method header itself)."
        }
    })
}

pub fn catalog() -> SkillCatalog {
    SkillCatalog {
        version: env!("CARGO_PKG_VERSION"),
        skills: vec![
            // ---- Bundle inspection -----------------------------------
            Skill {
                name: "inspect",
                description: "Top-level summary of a bundle: kind (apk/ipa/native), label, content hash, and one row per artifact with id, size, architecture, section count.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": path_arg() }
                }),
                output_shape: json!({
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string"},
                        "label": {"type": "string"},
                        "bundle_id": {"type": ["string","null"]},
                        "source_path": {"type": "string"},
                        "artifacts": {"type": "array"}
                    }
                }),
                example: "glass inspect ./app.apk",
            },
            Skill {
                name: "artifacts",
                description: "Flat artifact list (same rows as `inspect`, no bundle header).",
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": path_arg() }
                }),
                output_shape: json!({ "type": "array" }),
                example: "glass artifacts ./app.apk",
            },
            Skill {
                name: "sections",
                description: "Per-artifact section table (name, kind, address, size, bytes-on-disk). Pass `artifact` to narrow to one.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": { "type": "string", "description": "Optional artifact filter — label or hex prefix." }
                    }
                }),
                output_shape: json!({ "type": "array" }),
                example: "glass sections ./libfoo.so",
            },
            Skill {
                name: "binary-info",
                description: "Per-artifact binary format / architecture / raw section + symbol counts.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": path_arg() }
                }),
                output_shape: json!({ "type": "array" }),
                example: "glass binary-info ./libfoo.so",
            },
            Skill {
                name: "hash",
                description: "Content-hash a file in isolation. Returns artifact_id, byte size, elapsed time. Doubles as a hashing benchmark.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": path_arg() }
                }),
                output_shape: json!({
                    "type": "object",
                    "properties": {
                        "artifact_id": {"type":"string"},
                        "size_bytes": {"type":"integer"},
                        "duration_ms": {"type":"integer"}
                    }
                }),
                example: "glass hash ./libfoo.so",
            },

            // ---- Symbols --------------------------------------------
            Skill {
                name: "symbols",
                description: "List symbols across one or all artifacts. Filter by substring on the demangled name, by kind (function/object/other), and cap per artifact.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": { "type": "string", "description": "Optional artifact filter." },
                        "filter": { "type": "string", "description": "Case-insensitive substring on the demangled name." },
                        "kind": { "type": "string", "enum": ["function","object","other"] },
                        "limit": { "type": "integer", "minimum": 1 }
                    }
                }),
                output_shape: json!({ "type": "array" }),
                example: "glass symbols ./libfoo.so --filter init --kind function --limit 20",
            },
            Skill {
                name: "symbol-at",
                description: "Symbol covering / at a hex address. Returns null when no symbol covers the address.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","artifact","addr"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "addr": hex_addr_arg()
                    }
                }),
                output_shape: json!({ "type": ["object","null"] }),
                example: "glass symbol-at ./libfoo.so 0x1000058d4 --artifact libfoo.so",
            },
            Skill {
                name: "demangle",
                description: "Run one symbol through the C++/Rust/Swift demangler. No bundle required.",
                input_schema: json!({
                    "type": "object",
                    "required": ["name"],
                    "properties": {
                        "name": { "type": "string", "description": "Mangled symbol name (e.g. _ZN5glass4mainE)." }
                    }
                }),
                output_shape: json!({
                    "type": "object",
                    "properties": {
                        "input": {"type":"string"},
                        "demangled": {"type":"string"}
                    }
                }),
                example: "glass demangle _ZN5glass4mainE",
            },

            // ---- Disasm ---------------------------------------------
            Skill {
                name: "disasm",
                description: "Linear-sweep disassembly of a text section. Each row has address, raw bytes, mnemonic, operands, the covering symbol, and a resolved branch / ADRP target comment.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","artifact"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "section": { "type": "string", "description": "Optional section name (e.g. '.text', '__text'). When omitted, picks the first text section." },
                        "limit": { "type": "integer", "minimum": 1 }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass disasm ./libfoo.so --artifact libfoo.so --limit 100",
            },
            Skill {
                name: "decode",
                description: "Decode one 32-bit AArch64 instruction word. No bundle required. `addr` matters for PC-relative branch decoding.",
                input_schema: json!({
                    "type": "object",
                    "required": ["word"],
                    "properties": {
                        "word": { "type": "string", "description": "Hex instruction word (e.g. '0x52800000')." },
                        "addr": { "type": "string", "description": "Instruction address as hex; defaults to 0." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass decode 0x52800000",
            },

            // ---- CFG ------------------------------------------------
            Skill {
                name: "cfg-of",
                description: "Build the control-flow graph (blocks, edges, layout) for one function. Accepts hex address or exact symbol name for `func`.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","artifact","func"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "func": { "type": "string", "description": "Function entry — hex address or exact symbol name (e.g. 'glass::main')." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass cfg-of ./libfoo.so --artifact libfoo.so --func \"glass::main\"",
            },
            Skill {
                name: "calls-from",
                description: "Every call site inside a function. Lighter than `cfg-of` when you only need the outbound call list.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","artifact","func"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "func": { "type": "string", "description": "Function entry — hex address or exact symbol name." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass calls-from ./libfoo.so --artifact libfoo.so --func _main",
            },

            // ---- DEX / smali ----------------------------------------
            Skill {
                name: "classes",
                description: "List DEX classes (APK only). Optional `package` filters by JNI or Java prefix.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": {
                        "path": path_arg(),
                        "package": { "type": "string", "description": "Optional prefix filter — JNI ('Lkotlin/') or Java ('kotlin.')." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass classes ./app.apk --package androidx.annotation.",
            },
            Skill {
                name: "smali",
                description: "Full smali source for one class.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","class"],
                    "properties": {
                        "path": path_arg(),
                        "class": class_arg()
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass smali ./app.apk --class com.example.MainActivity",
            },
            Skill {
                name: "methods",
                description: "Methods declared by a class (name, descriptor, modifiers, op count, constructor flag).",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","class"],
                    "properties": {
                        "path": path_arg(),
                        "class": class_arg()
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass methods ./app.apk --class com.example.MainActivity",
            },
            Skill {
                name: "fields",
                description: "Fields declared by a class (name, type, modifiers).",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","class"],
                    "properties": {
                        "path": path_arg(),
                        "class": class_arg()
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass fields ./app.apk --class com.example.Foo",
            },
            Skill {
                name: "method-calls",
                description: "Every `invoke-*` call site inside a method. `method` is a bare name (first match) or 'name(descriptor)' for unambiguous lookup.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","class","method"],
                    "properties": {
                        "path": path_arg(),
                        "class": class_arg(),
                        "method": { "type": "string", "description": "Method name, or 'name(descriptor)' (e.g. 'bar(Ljava/lang/String;)V')." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass method-calls ./app.apk --class com.example.Foo --method 'bar(Ljava/lang/String;)V'",
            },

            // ---- Xref ------------------------------------------------
            Skill {
                name: "xref-addr",
                description: "Native callers and address-takes (direct branches + ADRP/ADD pairs) pointing at `addr` inside one artifact's text sections.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","artifact","addr"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "addr": hex_addr_arg()
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass xref-addr ./libfoo.so --artifact libfoo.so 0x1000058d4",
            },
            Skill {
                name: "callers",
                description: "Same as `xref-addr` but accepts a symbol name. Convenience wrapper for 'who calls X?'.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","artifact","symbol"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "symbol": { "type": "string", "description": "Symbol display name or raw name." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass callers ./libfoo.so --artifact libfoo.so --symbol \"glass::main\"",
            },
            Skill {
                name: "dex-callers",
                description: "DEX methods that `invoke-*` the given method key (smali form, Lclass;->name(descriptor)return).",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","method"],
                    "properties": {
                        "path": path_arg(),
                        "method": { "type": "string", "description": "Method key in smali form, e.g. 'Lcom/example/Foo;->bar()V'." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass dex-callers ./app.apk --method 'Lcom/example/Foo;->bar()V'",
            },
            Skill {
                name: "field-refs",
                description: "DEX methods that read or write the given field (iget/iput/sget/sput call sites).",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","field"],
                    "properties": {
                        "path": path_arg(),
                        "field": { "type": "string", "description": "Field reference in smali form, e.g. 'Ljava/lang/System;->out:Ljava/io/PrintStream;'." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass field-refs ./app.apk --field 'Ljava/lang/System;->out:Ljava/io/PrintStream;'",
            },

            // ---- Search / strings ------------------------------------
            Skill {
                name: "search",
                description: "Case-insensitive substring search across native symbols + DEX class/method/field names. Returns kind, label, context, and a jump target (hex address for native, JNI form for DEX).",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","query"],
                    "properties": {
                        "path": path_arg(),
                        "query": { "type": "string", "description": "Search term — substring, case-insensitive." },
                        "limit": { "type": "integer", "minimum": 1 }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass search ./app.apk onCreate --limit 20",
            },
            Skill {
                name: "strings",
                description: "Printable-ASCII NUL-terminated strings from a native artifact's non-text non-debug sections.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path","artifact"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "min": { "type": "integer", "minimum": 1, "description": "Minimum string length. Default 4." },
                        "limit": { "type": "integer", "minimum": 1 }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass strings ./libfoo.so --artifact libfoo.so --min 8",
            },

            // ---- Annotations / DB ------------------------------------
            Skill {
                name: "annotations",
                description: "Read user-set rename / comment / colour annotations for the artifact identified by content-hashing `path`. Empty list when no record exists.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": path_arg() }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass annotations ./libfoo.so",
            },
            Skill {
                name: "db-dump",
                description: "Read the bundle-level record (open tabs, expanded paths, last-opened time) for the file at `path`. Returns record: null when the bundle has never been opened.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": path_arg() }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass db-dump ./app.apk",
            },

            // ---- Annotation writes ----------------------------------
            Skill {
                name: "set-rename",
                description: "Persist a user-chosen display name for an address / symbol / class / method. Merges with any existing comment / colour on the same key — they are not overwritten. Annotations follow the artifact (content-hash), so the same libfoo.so in two bundles shares names.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "key_kind", "key", "name"],
                    "properties": {
                        "path": path_arg(),
                        "key_kind": annotation_key_props()["key_kind"].clone(),
                        "key": annotation_key_props()["key"].clone(),
                        "method": annotation_key_props()["method"].clone(),
                        "name": { "type": "string", "description": "New display name (free text)." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass set-rename ./libfoo.so --key-kind address --key 0x1000058d4 --name decode_packet",
            },
            Skill {
                name: "set-comment",
                description: "Attach a free-text comment to an address / symbol / class / method. Merges with any existing rename / colour on the same key — they are not overwritten.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "key_kind", "key", "body"],
                    "properties": {
                        "path": path_arg(),
                        "key_kind": annotation_key_props()["key_kind"].clone(),
                        "key": annotation_key_props()["key"].clone(),
                        "method": annotation_key_props()["method"].clone(),
                        "body": { "type": "string", "description": "Comment body (free text, multi-line OK)." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass set-comment ./libfoo.so --key-kind symbol --key glass::main --body \"entrypoint after rustc demangle\"",
            },
            Skill {
                name: "set-colour",
                description: "Tag an address / symbol / class / method with an RGBA colour. UI uses this as a row / node tint. Merges with any existing rename / comment on the same key — they are not overwritten.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "key_kind", "key", "rgba"],
                    "properties": {
                        "path": path_arg(),
                        "key_kind": annotation_key_props()["key_kind"].clone(),
                        "key": annotation_key_props()["key"].clone(),
                        "method": annotation_key_props()["method"].clone(),
                        "rgba": { "type": "string", "description": "RGBA hex (8 digits, with or without 0x). E.g. 'ff0000aa' = semi-transparent red." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass set-colour ./libfoo.so --key-kind address --key 0x1000058d4 --rgba ff0000aa",
            },
            Skill {
                name: "clear-annotation",
                description: "Remove any annotation hung off a given key. No-op if no annotation exists.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "key_kind", "key"],
                    "properties": {
                        "path": path_arg(),
                        "key_kind": annotation_key_props()["key_kind"].clone(),
                        "key": annotation_key_props()["key"].clone(),
                        "method": annotation_key_props()["method"].clone()
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass clear-annotation ./libfoo.so --key-kind address --key 0x1000058d4",
            },

            // ---- Binary pattern search ------------------------------
            Skill {
                name: "bin-search",
                description: "Scan a native artifact's text + data sections for a byte pattern. Atoms are space-separated; each is either a 2-char byte mask (`c0`, `0xc0`, `e?`, `?f`, `??`) or a gap (`*` = 0..=32 bytes, `*(min..max)` for explicit bounds). Matches don't span sections. Results carry a preview: two decoded AArch64 instructions joined with ` ; ` for text sections, first 8 bytes as hex for data. Use for finding code shapes (e.g. ADRP+ADD), well-known prologues, magic numbers, or any byte sequence.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "artifact", "pattern"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "pattern": {
                            "type": "string",
                            "description": "Pattern grammar: `c0` (literal byte), `e?` / `?f` / `??` (nibble wildcards), `*` (default 0..=32 byte gap), `*(min..max)` (explicit gap range). AArch64 bytes are in file order (little-endian word), e.g. `mov w0, #1; ret` = `20 00 80 52 c0 03 5f d6`."
                        },
                        "section": { "type": "string", "description": "Optional: narrow to one section by name (e.g. `__text`)." },
                        "limit": { "type": "integer", "minimum": 1, "description": "Cap on returned matches across all sections." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass bin-search ./libfoo.so --artifact libfoo.so --pattern '00 00 80 d2 c0 03 5f d6'",
            },
            Skill {
                name: "insn-search",
                description: "Search for a sequence of AArch64 instructions. The pattern is a `;`-separated assembly sequence (e.g. `mov w0, #1 ; ret`). Each instruction is encoded via armv8-encode and the resulting bytes — with operand-bit masking for any wildcards — flow into the byte-search engine. Higher-level than `bin-search` when you want to express a code shape without working out the exact bits. Wildcards (Phase C): bare `*` matches any operand; `#*` hints an immediate; bare `x` or `w` matches any X- or W-class register. Bracketed forms `<*>`, `<X>`, `<W>`, `<imm>` work too — useful when the wildcard is embedded inside other syntax (e.g. `[x, #*]`). Captures (`<name:kind>` cross-referencing the same wildcard later) are not yet implemented. Concrete operands constrain the encoding directly. The compiled pattern is shown back in the response as `bytes_hex` (e.g. `01/1f ?? ?? 90/9f` for `adrp x1, *`) for debugging.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "artifact", "pattern"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "pattern": {
                            "type": "string",
                            "description": "Semicolon-separated AArch64 instructions with optional wildcards. Examples: `mov w0, #1 ; ret` (fully concrete); `adrp x1, *` (Rd=1 fixed, target address wildcarded); `mov x, #*` (any X-register, any immediate); `ldr x, [x, #*]` (load any X via any X base + any offset). Mnemonic + operand count must match an opcode in the AArch64 table; ranking prefers opcodes whose slot kinds match wildcard hints (so `#*` lands on immediate forms rather than aliases that hide an immediate elsewhere)."
                        },
                        "section": { "type": "string", "description": "Optional: narrow to one section by name (defaults to all text sections)." },
                        "limit": { "type": "integer", "minimum": 1, "description": "Cap on returned matches across all sections." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass insn-search ./libfoo.so --artifact libfoo.so --pattern 'adrp x1, * ; add x1, x1, #*'",
            },

            // ---- Patching --------------------------------------------
            Skill {
                name: "patch",
                description: "Stage one instruction or byte edit in a patch file. The file accumulates edits across calls (read-modify-write JSON), and is consumed by `export-patched` to write a patched bundle. Provide exactly one of `insn` (AArch64 assembly source) or `bytes` (raw hex pairs). Same `(artifact, addr)` appearing twice replaces the earlier edit. Patch-file schema: `{version: 1, source_path?: string, edits: [{artifact: <64-char hex>, vaddr: <u64>, kind: \"Instruction|Bytes|String\", new_bytes: [<u8>...], original_bytes?: [<u8>...], source_text?: string}]}`. Use `patch-schema` to fetch the full JSON Schema.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "artifact", "addr", "patches"],
                    "properties": {
                        "path": path_arg(),
                        "artifact": artifact_arg(),
                        "addr": hex_addr_arg(),
                        "insn": { "type": "string", "description": "AArch64 assembly source for a single instruction, e.g. 'mov w0, #1' or 'ret'. Mutually exclusive with `bytes`." },
                        "bytes": { "type": "string", "description": "Raw replacement bytes as space-separated hex pairs (e.g. '20 00 80 52'). Length must match the original at addr (typically 4 for instructions). Mutually exclusive with `insn`." },
                        "patches": { "type": "string", "description": "Path to a patch file (JSON). Created if absent; rewritten on each call." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass patch ./libfoo.so --artifact libfoo.so --addr 0x1000058d4 --insn 'mov w0, #1' --patches /tmp/p.json",
            },
            Skill {
                name: "export-patched",
                description: "Apply a patch file to a bundle and write the patched output. For APK/AAB this re-packs the zip with the patched native libs; for IPA it re-streams the archive; for standalone binaries it just writes the patched bytes. Errors if the patch file is empty.",
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "patches", "out"],
                    "properties": {
                        "path": path_arg(),
                        "patches": { "type": "string", "description": "Patch file (JSON) produced by one or more `patch` calls." },
                        "out": { "type": "string", "description": "Destination path for the patched bundle. Parent directory created if needed." }
                    }
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass export-patched ./libfoo.so --patches /tmp/p.json --out ./libfoo-patched.so",
            },
            Skill {
                name: "patch-schema",
                description: "Print the JSON Schema (draft 2020-12) for the patch file format. Useful for external validators or tooling that wants to construct patch files programmatically.",
                input_schema: json!({
                    "type": "object",
                    "properties": {}
                }),
                output_shape: json!({ "type": "object" }),
                example: "glass patch-schema",
            },
        ],
    }
}
