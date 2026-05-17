# Automation API — design sketch

A surface for driving Glass without the GUI. Two flavours from the
same underlying capability set:

1. **CLI** — every capability mapped to a `glass <verb>` subcommand
   that writes structured output (JSON by default, optional `--text`
   for human-friendly) to stdout. Suitable for shell pipelines,
   CI checks, batch analysis runs.
2. **JS scripting API** — same capabilities exposed as functions on
   a `glass.*` namespace, callable from QuickJS scripts loaded by
   `glass-script`. Scripts can be invoked from the GUI (menus), from
   the CLI (`glass run script.js [args...]`), or in headless batch
   mode (`glass batch script.js bundle1.apk bundle2.apk`).

The CLI is the **first** target — it's the API surface boiled down
to its simplest possible shape. The JS layer wraps the same
capability layer but adds: object-graph navigation, tab/window-state
mutation (in GUI mode), and the ability to chain operations without
re-parsing the bundle every time.

## Design principles

- **Structured output by default.** Every CLI command writes JSON to
  stdout. `--text` opts into a human-friendly format for the same
  data. No stderr-vs-stdout shenanigans (logs → stderr, results →
  stdout, errors → stderr + non-zero exit).
- **Stateless commands first.** Each CLI invocation re-opens the
  bundle and runs to completion. No daemon. Cheaper to reason about,
  trivially scriptable.
- **Indices on demand.** The xref / symbol / search indices are
  built lazily on first use in CLI mode (whereas the GUI builds
  them eagerly after load). `--indices=all` forces the GUI's
  eager-build behaviour.
- **JS sits on the same capability layer.** No CLI-specific logic
  the JS host has to reinvent. A JS call `glass.symbols(...)` and
  a CLI call `glass symbols` resolve to the same Rust function with
  the same arguments and the same return shape.

## Naming + arg patterns

| | |
|--|--|
| **Verb** | imperative, lowercase, hyphen-separated for multi-word. `glass symbols`, `glass disasm`, `glass find-refs`. |
| **Target** | positional path to bundle / binary, or `--bundle <id>` to address an already-known artifact by content-hash. |
| **Scope** | `--artifact <id-or-name>` and/or `--section <name>` to narrow into a multi-artifact bundle. Optional; defaults to "all artifacts" where it makes sense. |
| **Filter** | `--filter <regex>`, `--prefix <str>`, `--limit <N>`, `--kind <enum>` — composable, all optional. |
| **Format** | `--format json|text|csv|ndjson` (default `json`). |
| **Persistence** | CLI commands don't write to the GUI's `redb` by default. `--persist-annotations` opts in (useful for analysis scripts that mark up the binary for later GUI sessions). |

## Exit codes

- `0` success.
- `1` general failure (bad input, parse error).
- `2` usage error (bad flags).
- `3` not found (symbol doesn't exist, section missing, bundle not in DB).
- `4` indexing in progress and `--no-wait` was passed.

## Capability table

The table below maps every capability the codebase currently
implements (or has obvious building blocks for) to a proposed CLI
verb and a JS API call. Rows marked **MVP** are the first cut;
**stretch** are obvious extensions that don't add much new code.

### Bundle / binary inspection

| Capability | CLI | JS | Tier |
|---|---|---|---|
| Identify a bundle (BundleId, label, artifact list) | `glass inspect <path>` | `glass.inspect(path)` | MVP |
| List artifacts in a bundle | `glass artifacts <path>` | `glass.artifacts(path)` | MVP |
| Read AndroidManifest as structured rows | `glass manifest <apk>` | `glass.manifest(apk)` | MVP |
| Read Info.plist as structured key/value | `glass info-plist <ipa>` | `glass.infoPlist(ipa)` | MVP |
| Per-artifact section table | `glass sections <path> --artifact <id>` | `glass.sections(path, opts)` | MVP |
| Native binary kind (ELF / Mach-O / fat / thin) | `glass binary-info <path>` | `glass.binaryInfo(path)` | MVP |
| List embedded frameworks (IPA) | `glass frameworks <ipa>` | `glass.frameworks(ipa)` | MVP |
| List DEX files in an APK | `glass dex-files <apk>` | `glass.dexFiles(apk)` | MVP |
| Hash bench / content-id | `glass hash <path>` *(replaces existing `hash-bench`)* | `glass.hash(path)` | MVP |

### Symbols

| Capability | CLI | JS | Tier |
|---|---|---|---|
| List symbols in an artifact | `glass symbols <path> --artifact <id> [--filter] [--kind]` | `glass.symbols(path, opts)` | MVP |
| Look up symbol covering an address | `glass symbol-at <path> <addr> --artifact <id>` | `glass.symbolAt(path, addr, opts)` | MVP |
| Demangle a single mangled name | `glass demangle <name>` | `glass.demangle(name)` | MVP |
| Symbol sources breakdown (symtab/DWARF/PLT/etc.) | `glass symbols --show-sources` | included in symbols payload | MVP |

### Disassembly (AArch64)

| Capability | CLI | JS | Tier |
|---|---|---|---|
| Linear sweep of a text section | `glass disasm <path> --artifact <id> --section <name>` | `glass.disasm(path, opts)` | MVP |
| Single-function listing rows (with resolved comments + arrows) | `glass disasm <path> --func <addr-or-name>` | `glass.disasmFunction(path, ref)` | MVP |
| Decode one word (for ad-hoc inspection) | `glass decode <hex-word>` | `glass.decode(word)` | MVP |
| Resolve ADRP+ADD string literal at address | `glass peek-string <path> <addr>` | `glass.peekString(path, addr)` | MVP |
| Linear sweep with `--max-rows N` for sampling | added flag | as above | MVP |

### Control-flow graph

| Capability | CLI | JS | Tier |
|---|---|---|---|
| Build CFG for a function | `glass cfg <path> --func <ref>` *(replaces existing)* | `glass.cfg(path, funcRef)` | MVP |
| Get basic-block list + edges | included in `cfg` payload | included | MVP |
| Layout coordinates (rank / x) | included in `cfg` payload | included | MVP |
| Call sites for a function | `glass calls-from <path> --func <ref>` | `glass.callsFrom(path, ref)` | MVP |

### DEX / smali

| Capability | CLI | JS | Tier |
|---|---|---|---|
| List classes (with optional package filter) | `glass classes <apk> [--package <prefix>]` | `glass.classes(apk, opts)` | MVP |
| Smali source of one class | `glass smali <apk> --class <jni>` | `glass.smali(apk, jni)` | MVP |
| List methods of a class | `glass methods <apk> --class <jni>` | `glass.methods(apk, jni)` | MVP |
| List fields of a class | `glass fields <apk> --class <jni>` | `glass.fields(apk, jni)` | MVP |
| Method call graph (caller → callees) | `glass method-calls <apk> [--method <key>]` | `glass.methodCalls(apk, opts)` | MVP |

### Cross-references (xref)

| Capability | CLI | JS | Tier |
|---|---|---|---|
| Native xrefs to address | `glass xref-addr <path> <addr> --artifact <id>` | `glass.xrefAddress(path, addr, opts)` | MVP |
| Callers of a native function | `glass callers <path> --func <ref>` | `glass.callers(path, ref)` | MVP |
| Callers of a DEX method | `glass dex-callers <apk> --method <key>` | `glass.dexCallers(apk, key)` | MVP |
| DEX field references | `glass field-refs <apk> --field <key>` | `glass.fieldRefs(apk, key)` | MVP |

### Search

| Capability | CLI | JS | Tier |
|---|---|---|---|
| Full project fuzzy search | `glass search <path> <query> [--limit N]` | `glass.search(path, query, opts)` | MVP |
| Strings in data sections | `glass strings <path> [--artifact <id>] [--min-len N]` | `glass.strings(path, opts)` | MVP |
| Search by symbol-kind / scope | filter flags on `search` | options bag | MVP |
| Pattern search (`adrp ?; add ?, ?, ?`) | `glass pattern <path> <pat>` | `glass.pattern(path, pat)` | stretch |

### Annotations / persistence (writes)

| Capability | CLI | JS | Tier |
|---|---|---|---|
| Read annotations for a bundle | `glass annotations <path>` | `glass.annotations(path)` | MVP |
| Add / overwrite an annotation | `glass annotate <path> <key> <json>` | `glass.annotate(path, key, value)` | stretch |
| Delete an annotation | `glass annotate-del <path> <key>` | `glass.deleteAnnotation(path, key)` | stretch |
| Bookmarks (read / add / remove) | `glass bookmarks <path>`, `glass bookmark add/rm` | `glass.bookmarks(...)` | stretch |
| Dump full persisted record | `glass db-dump <path>` *(existing)* | `glass.dbDump(path)` | MVP |

### GUI integration (JS only — these don't make sense as CLI)

The JS host has a richer surface when it's running inside the GUI
process. CLI-side these calls either return immediately with a
"no GUI" error or no-op.

| Capability | JS | Tier |
|---|---|---|
| Open / focus a Listing tab at an address | `glass.gui.openListing(artifact, section, addr)` | MVP |
| Open a Hex tab | `glass.gui.openHex(artifact, section, addr)` | MVP |
| Open a CFG tab | `glass.gui.openCfg(artifact, entryAddr)` | MVP |
| Open a smali tab at a method | `glass.gui.openSmali(jni, line)` | MVP |
| Show a notification / toast | `glass.gui.notify(text)` | stretch |
| Prompt the user (text input / confirm) | `glass.gui.prompt(question)` | stretch |
| Register a menu item | `glass.gui.registerMenu(name, callback)` | stretch |

### Script lifecycle

| Capability | CLI | JS | Tier |
|---|---|---|---|
| Run a single script against a bundle | `glass run script.js <path> [args]` | n/a | MVP |
| Batch run against many bundles | `glass batch script.js <path>...` | n/a | stretch |
| Script announces title / shortcut for GUI | n/a | top-level `export const meta = { ... }` | stretch |
| Read stdin in script (line-by-line) | n/a | `for await (const line of glass.stdin())` | stretch |

## CLI output shape — JSON conventions

- One canonical key for results: `"data"`. Errors go in `"error"`.
- Top-level objects, not arrays, so we can attach metadata (timing, total counts) without breaking parsers.
- `--format ndjson` switches to one JSON object per line for large list responses (good for piping into `jq` / Unix-fu).
- Addresses are emitted as `"0x..."` strings to avoid JSON number-precision loss on > 2^53 values.

Sample (`glass symbols /libfoo.so --limit 3`):

```json
{
  "data": {
    "artifact": "blake3:abc123…",
    "total": 4827,
    "shown": 3,
    "symbols": [
      { "address": "0x1000a0", "size": 64, "name": "_start", "demangled": "_start", "kind": "Function", "sources": ["SYMTAB"] },
      { "address": "0x100100", "size": 128, "name": "_ZN3foo3barEv", "demangled": "foo::bar()", "kind": "Function", "sources": ["SYMTAB"] },
      …
    ]
  },
  "meta": { "duration_ms": 42 }
}
```

## JS API shape — call conventions

- Every top-level function is sync from the script's POV; the host
  blocks the script's event loop until the result is ready. Reason:
  scripts are usually short and analytical, not UI-driven. Heavy
  ops (xref index build) report progress via an `onProgress` opt.
- Object returns rather than tuples — `{ data, meta }` mirrors CLI.
- Bundle handles get returned once and chained: `const bundle = glass.open(path); const syms = bundle.symbols({ filter: "foo" });` — saves re-parsing.
- Addresses as JS strings (`"0x100a4"`) for the same overflow reason as the CLI.

Sample script:

```js
import { open } from 'glass';

export default function (argv) {
  const bundle = open(argv[0]);
  const exported = bundle.symbols({ kind: 'Function' })
    .data.symbols.filter(s => s.sources.includes('SYMTAB'));

  for (const s of exported) {
    const xrefs = bundle.callers({ func: s.address }).data;
    if (xrefs.length === 0) {
      console.log(`unreachable: ${s.demangled}`);
    }
  }
}

export const meta = {
  title: 'Find unreachable exports',
  shortcut: 'Cmd-Shift-U',
};
```

## Module boundaries

A single `glass-api` crate sits between the existing capability
crates and the consumers:

```
glass-cli                  glass-script                   glass-ui
  │                            │                             │
  ▼                            ▼                             ▼
       ╔════════════════ glass-api ════════════════╗
       ║  open / inspect / symbols / disasm / cfg  ║
       ║  classes / smali / xrefs / search /       ║
       ║  annotations / strings / patterns         ║
       ╚═══════════════════════════════════════════╝
            │              │           │
            ▼              ▼           ▼
       glass-arch-arm64   glass-arch-dex   glass-db
       glass-mobile       glass-core
```

`glass-api` re-exports the underlying types, builds the indices, and
runs xref / search / symbol queries. Both glass-cli and glass-script
become thin: glass-cli parses argv → dispatches → serialises to
JSON; glass-script binds JS functions → calls the same API → returns
to rquickjs.

## Scripting model — discovery, lifecycle, hooks

The CLI shape above covers "I want to ask Glass a question from
the command line." Scripts cover the more interesting case:
**user-supplied analyses that live inside the GUI**. Loaded
automatically at startup, hooked into menus / context actions, and
free to render their own results back into the window.

### Discovery

- Scripts live in `~/Library/Application Support/Glass/scripts/`
  (macOS) — one file per script, plain JS / TypeScript. Sub-
  directories allowed; the loader walks recursively.
- On launch, Glass scans the directory. For each `*.js` file:
  1. Parse + evaluate the module in its own QuickJS Realm.
  2. Call the module's `describe()` function. If absent or it
     throws, the script is marked failed; surfaced in the
     **Glass → Scripts…** dialog with the error.
  3. Cache the result keyed by file path + mtime. Re-runs only
     when the file changes (live edit-and-reload).
- The scripts directory ships with a sample script per hook kind
  so new users have something to crib from.

### `describe()` — the metadata contract

Returns a single object describing what the script wants to be:

```js
export function describe() {
  return {
    name: "Find unreachable exports",
    description: "Symbols exported from .dynsym with zero xrefs.",
    version: "1.0.0",
    author: "azw",
    permissions: ["read"],          // see below
    hooks: [
      // Always-available menu item.
      { kind: "menu", path: "Scripts / Analyse / Unreachable exports" },
      // Per-view context-menu entry. The `when` field is a
      // boolean expression evaluated when the menu opens; "true"
      // means always show.
      { kind: "context", view: "listing", label: "Find unreachable exports", when: "selection.kind == 'function'" },
      // Keyboard shortcut. Activatable from anywhere.
      { kind: "shortcut", binding: "cmd-shift-u" },
      // Lifecycle. Fires once per bundle load, before the user
      // can interact. Heavy work should defer to `idle`.
      { kind: "lifecycle", event: "bundle-loaded" },
    ],
    // Optional. When omitted the script doesn't produce output —
    // useful for pure side-effects like annotating a bundle.
    output: { kind: "pane", title: "Unreachable exports" },
  };
}
```

Hook kinds:

| Kind | Args bag passed to `execute()` | Notes |
|---|---|---|
| `menu` | `{ kind: "menu" }` | Top-level / submenu entry. `path` is `/`-separated; Glass creates the submenu hierarchy. |
| `context` | `{ kind: "context", view, target }` | `view` = `listing`/`hex`/`smali`/`cfg`/`dex-cg`/`overview`. `target` is view-specific (address + artifact for listing, method key for smali, etc.). |
| `shortcut` | `{ kind: "shortcut", chord }` | Bound globally. Conflicts with built-in shortcuts → script fails to register with a clear error. |
| `lifecycle` | `{ kind: "lifecycle", event, bundleId }` | `event` ∈ `app-launch`, `bundle-loaded`, `tab-open`, `tab-close`, `app-quit`. Run on the main thread by default; opt into background by returning a promise. |

### `execute(args)` — the work

Glass calls `execute(args)` synchronously by default. The script
can:

- Read from the bundle via `glass.*` (whichever capabilities its
  declared `permissions` allow).
- Emit content into its declared `output` channel.
- Trigger GUI actions: open a tab, focus a window, show a toast.

Output kinds:

| `output.kind` | Behaviour |
|---|---|
| `pane` | A new side panel (or focused if already open) named `title`. Script populates it via `glass.output.push(element)`. Cleared at the start of each `execute()` unless `accumulate: true`. |
| `tab` | A new transient tab kind. Same `push()` API; persists across tab switches but not across app restarts. |
| `console` | Equivalent to CLI mode — emits JSON / text the user can copy. Surfaced in a small modal. |
| (omitted) | No output channel; the script is pure side-effect (annotates, opens a built-in view, etc.). |

`glass.output.push(...)` takes a structured element — not raw
HTML. The element vocabulary maps to gpui primitives so we don't
have to ship an HTML renderer:

```js
glass.output.push({ kind: "heading", text: "Methods calling Cipher.doFinal" });
glass.output.push({
  kind: "table",
  header: ["Class", "Method", "Call site"],
  rows: matches.map(m => [
    { text: m.class, link: { kind: "smali", jni: m.classJni } },
    { text: m.method },
    { text: m.callerKey, link: { kind: "smali", jni: m.classJni, line: m.line } },
  ]),
});
glass.output.push({ kind: "code", language: "smali", text: snippet });
glass.output.push({ kind: "log", level: "warn", text: "47 methods skipped (no body)" });
```

Element kinds (MVP): `heading`, `text`, `table`, `code`, `log`,
`divider`, `link`. `link.kind` reuses the same `SearchJump` enum
as the palette so navigation behaviour is identical.

### Lifecycles, end-to-end

#### Script that adds a context-menu entry

```
app launch:
  walk scripts dir → for each *.js:
    parse + eval module in fresh Realm
    call describe()
    register declared hooks
      e.g. context-menu hook "Find unreachable exports" on listing view

user opens a bundle, right-clicks an address in the disassembly:
  Glass builds the context menu
    evaluates each registered context hook's `when` expression
    inserts the matching ones at the bottom
  user picks "Find unreachable exports"
    Glass creates / focuses the output pane named "Unreachable exports"
    Glass calls script.execute({
      kind: "context",
      view: "listing",
      target: { artifact: "...", section: ".text", address: "0x100a4" },
    })
    script does its work, calls glass.output.push(...)
    Glass renders the pushed elements into the pane

next time user right-clicks: script's Realm is preserved → any
module-level state from the previous call is still there.
```

#### Lifecycle script that runs at load

```
user opens an APK:
  loader pipeline runs through to ShellState::Ready
  Glass fires the `bundle-loaded` lifecycle event
    for each script that registered for this event:
      Glass.spawn_background({
        script.execute({
          kind: "lifecycle",
          event: "bundle-loaded",
          bundleId: "blake3:...",
        })
      })
  watchdog: each script gets a 30s budget; exceeding it pauses
  it and surfaces a warning in the Scripts dialog.
```

#### Reload-on-edit during development

```
user edits ~/.../scripts/foo.js:
  fs-watcher fires
  Glass re-reads + re-evaluates the module in a new Realm
  describe() runs again → hooks re-registered (replacing the old ones)
  the next time the user invokes the script, the new code runs
```

### Permissions

`describe().permissions` is a coarse-grained capability declaration:

| Permission | Grants |
|---|---|
| `read` | Every read-only `glass.*` call. Default for new scripts. |
| `annotate` | Write paths into `redb`: annotations, bookmarks. |
| `gui` | Open / focus tabs, show notifications, prompt the user. |
| `network` | `glass.fetch(url)`. Behind a confirmation dialog on first use per script. |
| `fs` | Read / write outside the bundle (e.g. emit a report.csv next to the binary). |

A script tries to call something its declared permissions don't
cover → the binding throws a `PermissionDeniedError` the script
can catch + show in its UI. Glass also surfaces these in the
Scripts dialog so the user sees which scripts touched what.

The user can override per-script in **Glass → Scripts… → Permissions**.

### Worked examples — what's achievable

#### 1. Decryption-routine detector
*"Every smali method called `decrypt*` that calls a Cipher API."*
- `bundle-loaded` lifecycle hook.
- Read `glass.classes()` → for each class with matching methods,
  read `glass.smali()` → grep for `Ljavax/crypto/Cipher;->doFinal`
  / similar.
- Output a `table` pane with class / method / call-site links.

#### 2. JNI binding mapper
*"Show the JNI native function for each `native` smali method."*
- Context-menu entry on smali `.method native` lines.
- Read the method signature, apply JNI mangling, search native
  exports for the demangled name, open a Listing tab there.

#### 3. Anti-debug fingerprint
*"Walk every native lib, flag known anti-debug patterns."*
- Lifecycle `bundle-loaded` on APK / IPA.
- For each AArch64 text section: scan for `ptrace`, `__strncmp_chk`,
  `gettid`-comparing-against-known-debugger-pids patterns.
- Annotate matched addresses (`annotate` permission), push to
  the output pane.

#### 4. Symbol-export coverage report
*"How many of this binary's exports have actual callers?"*
- Menu entry.
- `glass.symbols({ kind: "Function" })` + `glass.callers(addr)` for each.
- Output: code-listing of uncalled exports + a count summary.

#### 5. Selective demangling
*"Demangle a single symbol with verbose output."*
- Shortcut `cmd-shift-d`, context-menu on a listing operand.
- `glass.demangle(name, { verbose: true })`.
- Output: `console` kind — small modal.

#### 6. CFG complexity scorer
*"Annotate each function with a cyclomatic-complexity score."*
- Lifecycle `bundle-loaded`.
- For each function: `glass.cfg()` → compute edges − nodes + 2.
- Annotate high-complexity functions; render a sortable table in
  the pane.

#### 7. String reference dumper
*"Every place in the binary that points at a string matching a
regex."*
- Menu entry; prompts for the regex via `glass.gui.prompt()`.
- `glass.strings()` filtered by regex → for each, `glass.xrefAddress()`.
- Output: table linking to each call site.

#### 8. DEX → native bridge map *(post cross-DEX↔native xrefs)*
*"Every Java method that's bound to a native symbol."*
- Lifecycle `bundle-loaded` on APK.
- `glass.jniBindings()` → table with two-column links (smali ↔ native).

### Risk + safety

- **Watchdog timer.** Each script invocation has a budget
  (default 30 s for lifecycle, 5 s for menu / context). Exceeding
  it pauses the QuickJS interpreter (rquickjs supports this).
  Pausing once → warning. Twice → script disabled until next
  manual reload.
- **Crash isolation.** A script throwing doesn't unwind into
  Glass; the host catches, surfaces the stack trace in the
  Scripts dialog, leaves the rest of the app alive.
- **No filesystem / network without declared permission.**
  See the permissions table above.
- **Disable / enable per script.** A simple checklist in the
  Scripts dialog. State persisted in `~/Library/Application
  Support/Glass/scripts/.disabled` (single file, one path per line)
  so the user can also flip it from outside Glass.
- **Reset module state.** A "Reset" button next to each script
  drops + re-creates its Realm — clears caches, runs `describe()`
  again. Handy when iterating.

### What scripts can't (yet) do

Out of scope for the first cut; document the gaps so script
authors know what's coming:

- **Persist results across app restarts** beyond what they
  annotate via the existing `redb` paths. A proper
  `glass.storage` key-value store is a follow-up.
- **Run after the app exits.** No daemon / scheduled-task model.
- **Modify the binary on disk.** Needs the in-place-edit writer
  from the roadmap; once that lands, scripts get
  `glass.patch(addr, bytes)` behind the `fs` permission.
- **Block on UI input across multiple steps.** A wizard-style
  multi-prompt flow needs an explicit Generator-style API; v1
  exposes only single `glass.gui.prompt(question)`.

## Open questions

- **Bundle handle lifetime in JS.** Single-script: bundle opens
  on first use, drops at script end. Multi-script daemon mode
  (`glass serve`): an LRU cache so common bundles stay parsed
  across script invocations. Daemon mode is post-MVP.
- **Progress reporting from CLI.** Long-running commands (xref
  build, disasm of huge libs) — emit progress to stderr or via
  a sideband JSON-lines stream on `--progress`? Default off.
- **JS bindings shape.** rquickjs gives us several options: hand-
  rolled `JsObject` accessors, or a serde-bridge style where
  the Rust types become JS objects automatically. Serde-bridge
  is faster to write but heavier at runtime; hand-rolled gives
  better error messages. Worth prototyping both on one verb.
- **Write paths.** Annotations and bookmarks go in `redb`; we
  want CLI writes to be safe to interleave with GUI writes. The
  existing `Database` flush model already supports this — needs a
  test or two to confirm.
- **Cross-DEX↔native binding xrefs** (deferred from the references
  feature). Once that lands, the CLI verb is `glass jni-bindings
  <apk>` and the JS call mirrors it.
- **Pattern language for instruction search**. Sketched in the
  roadmap but not designed. The CLI's `pattern` verb is a stretch
  item gated on that design.

## Next steps

1. Extract the `glass-api` crate. Initially re-export today's
   capability crates without behaviour changes — just a single
   entry point that consumers depend on.
2. Implement the MVP CLI verbs (the ~30 rows marked MVP above)
   one at a time. Each lands with its JSON schema documented in
   this doc. The existing CLI subcommands get replaced or removed.
3. Stand up the rquickjs host in `glass-script`. Start with a
   handful of bindings (`open`, `symbols`, `disasm`) to validate
   the FFI shape before scaling out.
4. Wire `glass run script.js` into the CLI for headless single-
   script execution.
5. Scripts directory + GUI integration:
   - Loader that walks `~/Library/Application Support/Glass/scripts/`,
     calls `describe()` per file, registers hooks.
   - Hook plumbing for `menu`, `context`, `shortcut`, `lifecycle`.
   - Output-pane gpui surface + the structured-element renderer.
   - Watchdog timer + crash isolation.
   - Permissions enforcement at the binding layer.
   - **Glass → Scripts…** management dialog (list, enable/disable,
     reset, view permissions).
6. Ship the per-hook sample scripts (one per row from "Worked
   examples" above) so users have working code to crib.
