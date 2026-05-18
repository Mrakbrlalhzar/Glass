# Glass CLI / Automation API

Every analysis that the Glass GUI performs is also exposed as a CLI
verb that emits structured JSON to stdout. The same `glass` binary
that opens the GUI is the automation entry point — pick a subcommand
and you get a one-shot, scriptable result.

This document covers every automation verb. Legacy text-output
subcommands (`arm64`, `bundle`, `gui`, `db-inject-tab`, `string-comments`,
`plt-probe`, `hash-bench`, `cfg`) predate the automation API and are
not described here; run `glass help <name>` for those.

## Conventions

**Output.** JSON by default. Pass `--text` for a human-readable
rendering — useful when you're reading at the terminal rather than
piping to `jq`.

```sh
glass inspect ./app.apk            # JSON
glass inspect ./app.apk --text     # human-readable
```

**Envelope.** Every JSON response has shape `{ "data": ..., "meta": { "duration_ms": ... } }`.
Errors go to stderr in the parallel shape `{ "error": { "message": ... } }`
and the process exits non-zero.

**Addresses.** Always hex strings (`"0x1000058d4"`), never raw
numbers — JS / `jq` lose precision past 2^53.

**Artifact references.** Verbs that take `--artifact` accept either
the artifact's full label (`arm64-v8a/libnative.so`, `glass`, the
framework name for an IPA) or any hex prefix of its content-hash ID
(`f46f`, `f46fac70…`). Use `glass artifacts <path>` to enumerate
them.

**Class references.** Verbs that take `--class` accept either JNI
form (`Lcom/example/Foo;`) or Java form (`com.example.Foo`).

**Method references.** Method keys are in smali form:
`Lclass;->name(descriptor)return`. `method-calls` accepts a bare
name when unambiguous; otherwise pass `name(descriptor)`.

## Global flags

| Flag        | Meaning                                                |
|-------------|--------------------------------------------------------|
| `--text`    | Render automation verbs as text instead of JSON        |
| `--fresh`   | (GUI only) ignore persisted tab / expansion state      |
| `--help`    | Show the subcommand list                               |

## Bundle inspection

### `inspect <path>` — top-level summary
Kind (`apk` / `ipa` / `native`), label, content hash, and one line
per artifact (id, size, architecture, section count).

```sh
glass inspect ./app.apk --text
```

### `artifacts <path>` — flat artifact list
Same artifact rows as `inspect`, no bundle header.

### `sections <path> [--artifact <ref>]`
Per-artifact section table (name, kind, address, size, bytes-on-disk).
`--artifact` narrows to one.

### `binary-info <path>`
Per-artifact format / architecture / section count / symbol-count hint.

### `hash <path>`
Content-hash a file in isolation — returns `artifact_id`, byte size,
elapsed time. Replaces the old `hash-bench` for benchmarking.

```sh
glass hash ./libfoo.so
# {"data":{"artifact_id":"f46f…","size_bytes":12345678,"duration_ms":42}, …}
```

## Symbols

### `symbols <path> [--artifact <ref>] [--filter <s>] [--kind <k>] [--limit <n>]`
Lists symbols across one or all artifacts. `--filter` is a
case-insensitive substring match on the demangled name. `--kind` is
one of `function`, `object`, `other`. `--limit` caps results per
artifact.

```sh
glass symbols ./libfoo.so --filter init --kind function --limit 20
```

### `symbol-at <path> <addr> --artifact <ref>`
Symbol covering / at a hex address. `addr` can be with or without
the `0x` prefix. Returns `null` when no symbol covers the address.

```sh
glass symbol-at ./libfoo.so 0x1000058d4 --artifact libfoo.so
```

### `demangle <name>`
Run one symbol name through the C++/Rust/Swift demangler. No bundle
required.

```sh
glass demangle _ZN5glass4mainE
```

## Disassembly

### `disasm <path> --artifact <ref> [--section <name>] [--limit <n>]`
Linear-sweep disassembly of a text section. When `--section` is
omitted, picks the first text section in the artifact. Each row
includes address, raw bytes, mnemonic, operands, the covering
symbol (if any), and a resolved branch / ADRP target comment.

```sh
glass disasm ./libfoo.so --artifact libfoo.so --section .text --limit 100
```

### `decode <word> [--addr <a>]`
Decode one 32-bit AArch64 word. `word` is hex; `addr` (default `0`)
matters for PC-relative branches. No bundle required.

```sh
glass decode 0x52800000        # mov w0, #0
glass decode 0x94000003 --addr 0x100000
```

## Control-flow graph

### `cfg-of <path> --artifact <ref> --func <ref>`
Block list + edges + layout for one function. `--func` accepts a
hex address or an exact symbol name.

```sh
glass cfg-of ./libfoo.so --artifact libfoo.so --func "glass::main"
```

Returns:
- `entry_address`, `end_address`
- `blocks[]` — id, start/end address, instruction count, call
  count, rank, x-coordinate (for layout), `exits_function` flag
- `edges[]` — from / to block id, kind (`Fallthrough` /
  `ConditionalTaken` / `Unconditional` / `Call` / `Return`)

### `calls-from <path> --artifact <ref> --func <ref>`
Every call site inside a function. Lighter than `cfg-of` if you
only want the outbound call list.

```sh
glass calls-from ./libfoo.so --artifact libfoo.so --func _main --text
```

## DEX / smali (APK only)

### `classes <path> [--package <prefix>]`
All DEX classes. `--package` filters by JNI or Java prefix.

```sh
glass classes ./app.apk --package androidx.annotation. --text
```

### `smali <path> --class <ref>`
Full smali source for one class.

```sh
glass smali ./app.apk --class com.example.MainActivity --text
```

### `methods <path> --class <ref>`
Methods declared by a class (name, descriptor, modifiers, op count,
constructor flag).

### `fields <path> --class <ref>`
Fields declared by a class (name, type, modifiers).

### `method-calls <path> --class <ref> --method <ref>`
Every `invoke-*` call site inside a method. `--method` is either a
bare name (first match) or `name(descriptor)` for unambiguous lookup.

```sh
glass method-calls ./app.apk --class com.example.Foo --method 'bar(Ljava/lang/String;)V'
```

## Cross-references

The xref verbs build their indices inline for each query — the CLI
is one-shot, so there's no cache to amortise. For sustained
interactive use, the GUI's incremental indices are faster.

### `xref-addr <path> --artifact <ref> <addr>`
Native callers and address-takes pointing at `<addr>` inside one
artifact's text sections. Catches direct branches, ADRP+ADD pairs,
and other PC-relative references.

```sh
glass xref-addr ./libfoo.so --artifact libfoo.so 0x1000058d4
```

### `callers <path> --artifact <ref> --symbol <name>`
Same as `xref-addr`, but accepts a symbol name. Convenience wrapper.

```sh
glass callers ./libfoo.so --artifact libfoo.so --symbol "glass::main"
```

### `dex-callers <path> --method <key>`
DEX methods that `invoke-*` the given method key (in smali form,
`Lclass;->name(descriptor)return`).

```sh
glass dex-callers ./app.apk --method 'Lcom/example/Foo;->bar()V'
```

### `field-refs <path> --field <ref>`
DEX methods that read or write the given field
(`iget*` / `iput*` / `sget*` / `sput*`).

```sh
glass field-refs ./app.apk --field 'Ljava/lang/System;->out:Ljava/io/PrintStream;'
```

## Search & strings

### `search <path> <query> [--limit <n>]`
Case-insensitive substring search across native symbols and DEX
class / method / field names. Each hit records its `kind`, `label`,
`context`, and a `jump` target (hex address for native, JNI form
for DEX).

```sh
glass search ./app.apk "onCreate" --limit 20
```

### `strings <path> --artifact <ref> [--min <n>] [--limit <n>]`
Printable-ASCII NUL-terminated strings from a native artifact's
non-text non-debug sections. Default `--min` is 4.

```sh
glass strings ./libfoo.so --artifact libfoo.so --min 8 --limit 50
```

## Binary pattern search

### `bin-search --path P --artifact A --pattern '...' [--section S] [--limit N]`
Scan every text + data section of one artifact for a byte
pattern. Atoms are whitespace-separated; each is either a
2-character byte mask (`c0`, `0xc0`, `e?`, `?f`, `??`) or a
bounded gap (`*` = 0..=32 bytes default, `*(min..max)` to
override).

```sh
# `mov w0, #1 ; ret`  (returning-true stub finder)
glass bin-search ./libfoo.so --artifact libfoo.so \
  --pattern '20 00 80 52 c0 03 5f d6'

# any ADRP+ADD pair with no intervening bytes
glass bin-search ./libfoo.so --artifact libfoo.so \
  --pattern '?? ?? ?? 9? ?? ?? 4? 91'

# raw data: find embedded magic
glass bin-search ./libfoo.so --artifact libfoo.so --pattern 'de ad be ef'
```

Matches don't span sections. Each result carries a `preview`
column: two decoded AArch64 instructions joined with ` ; ` for
text sections (e.g. `mov x0, #0 ; ret x30`), or the first 8
bytes as space-separated hex for data sections.

Full pattern reference + worked examples: [`docs/BinSearch.md`](BinSearch.md).

## Annotations & persistence

Glass persists window state, open tabs, and user annotations in a
content-addressed `redb` database. Annotations follow the artifact
(blake3 hash of the bytes), so the same `libfoo.so` shipped in two
different APKs shares analysis state.

Each annotation slot — keyed by `(artifact, key)` — carries up to
three independent facets: a **rename** (display name override), a
**comment** (free-form note), and a **colour** (RGBA tint). Writes
merge: setting a comment on a key that already has a rename leaves
the rename intact.

### `annotations <path>`
Read all annotations for the artifact identified by content-hashing
`<path>`. Each entry shows the populated facets — `rename` /
`comment` / `colour` (any combination).

### `set-rename --path P --key-kind K --key V [--method M] --name N`
Persist a display name. `--key-kind` is one of:
- `address` — `V` is a hex VA, e.g. `0x1000058d4`. Most specific.
- `symbol` — `V` is the symbol display name (`glass::main`).
- `class` — `V` is a class JNI (`Lcom/example/Foo;`).
- `method` — `V` is the class JNI; `--method` is the
  `name(descriptor)return` part, e.g. `bar(Ljava/lang/String;)V`.
- `method-line` — `V` is the class JNI; `--method` is
  `name(descriptor)return#<line_offset>` (line offset is
  0-indexed from the `.method` directive, so `0` targets the
  header itself, `1`+ targets a body line). This is the key
  the GUI writes when you right-click a specific line inside
  a smali method body.

```sh
glass set-rename ./libfoo.so --key-kind address --key 0x1000058d4 --name decode_packet
```

### `set-comment --path P --key-kind K --key V [--method M] --body B`
Free-form note on the same key. `--body` is multi-line OK.

```sh
glass set-comment ./libfoo.so --key-kind symbol --key "glass::main" \
  --body "entrypoint after rustc demangle"
```

### `set-colour --path P --key-kind K --key V [--method M] --rgba HEX`
RGBA hex (8 digits, with or without `0x`). UI renders this as a row /
node tint.

```sh
glass set-colour ./libfoo.so --key-kind address --key 0x1000058d4 --rgba ff0000aa
```

### `clear-annotation --path P --key-kind K --key V [--method M]`
Remove every facet hung off the key. No-op if nothing's stored.

### `db-dump <path>`
Reads the bundle-level record for the file at `<path>`: label,
schema version, last-opened time, artifact count, open tabs,
expanded paths, and the source path. Returns `record: null` when
the bundle has never been opened.

## Piping & composition

The JSON shape is stable and addresses-as-strings are safe for
`jq`. Some patterns:

```sh
# Extract just the addresses of all `init`-named symbols:
glass symbols ./libfoo.so --filter init --limit 20 \
  | jq -r '.data[].symbols[].address'

# Every basic block of glass::main in one line per block:
glass cfg-of ./libfoo.so --artifact libfoo.so --func "glass::main" \
  | jq -c '.data.blocks[]'

# Strings ≥ 16 chars, sorted by section:
glass strings ./libfoo.so --artifact libfoo.so --min 16 \
  | jq -r '.data.strings[] | "\(.section)\t\(.address)\t\(.value)"'

# Find DEX callers of every onCreate override:
for m in $(glass search ./app.apk onCreate \
             | jq -r '.data.hits[] | select(.kind=="method") | .jump'); do
  glass dex-callers ./app.apk --method "$m" --text
done
```

## Errors & exit codes

Failures emit a JSON object on **stderr** and exit with code 1:

```json
{"error":{"message":"no artifact matches \"libnope.so\""}}
```

With `--text`, the same message goes to stderr as `error: <msg>`.
Stdout is empty on failure, so `glass ... | jq` will simply produce
no output rather than swallowing a malformed line.

## Skill catalog & MCP

Two helper verbs expose the automation API to LLM tooling:

### `skills`
Prints the machine-readable catalog: one entry per verb with name,
description, JSON Schema for arguments, and an example invocation.
Useful for generating prompts, building external clients, or
verifying the surface programmatically.

```sh
glass skills | jq '.skills[] | {name, example}'
```

### `mcp`
Runs an MCP (Model Context Protocol) stdio server. Every verb in
this document becomes an LLM-callable tool with the schema shown
by `glass skills`. Plug into Claude Desktop, Cursor, Zed, or any
other MCP host.

```sh
glass mcp                 # speak JSON-RPC over stdin/stdout
```

For Claude Desktop, add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "glass": { "command": "/usr/local/bin/glass", "args": ["mcp"] }
  }
}
```

Tool results come back as a single text content block whose body
is the same `{ data, meta }` JSON envelope the CLI emits — parse
the `.content[0].text` field as JSON.

## See also

- [`docs/AutomationAPI.md`](AutomationAPI.md) — design notes for the
  capability surface (`glass-api` crate) that backs every verb.
- [`docs/Roadmap.md`](Roadmap.md) — what's planned next.
