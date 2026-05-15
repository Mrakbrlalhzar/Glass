# Glass

A fast, native, **mobile-app first** interactive disassembler. Spiritual successor to IDA Pro for the Android / iOS reverse engineering workflow, built around:

- `smali` for APK / DEX / smali handling
- `armv8-encode` for AArch64 (native `.so`, iOS Mach-O)
- `gpui` (Zed) for GPU-accelerated native UI
- `redb` for content-addressed persistence
- `rquickjs` for scriptable plugins (planned)

License: GPL-3.0-only (inherited from `smali`).


## Why?

We’ve all used IDA Pro — it’s the industry standard for reversing and has years of plugins behind it, but it’s slow, expensive, and dated. Glass is 100% Rust native with a GPU-accelerated UI for fluid interaction. It’s also 100% free and open source — please contribute.

## Status

Glass is usable today as an Android reversing tool for AArch64 native libraries and DEX bytecode. iOS (IPA / Mach-O) and 32-bit ARM are on the roadmap.

### What works

**Bundle loading**
- Open `.apk` files; loader pipeline reports progress (Reading archive → Parsing DEX → Building symbols).
- Per-artifact content-addressed IDs (blake3, rayon-parallel for large libs). Annotations follow the artifact, not the container — the same `libfoo.so` shipped in two APKs shares analysis state.
- AndroidManifest viewer (binary XML decoded via `smali`).

**AArch64 native (ELF + thin Mach-O)**
- Linear-sweep disassembly with virtualized rendering — large libraries open in seconds, not minutes.
- Symbol map merged from ELF symtab, dynsym, DWARF, `.eh_frame` FDEs, and synthesized `<name>@plt` entries. C++/Rust/Swift demangling via `symbolic-demangle`.
- Branch operands rendered as clickable symbol references; `adrp` + `add`/`ldr` pairs resolved to data targets, including string literals shown inline as comments.
- Per-section views (code sections get disassembly; data sections get a hex view).

**DEX / smali**
- Class tree across all DEX files in the APK.
- Smali listing per class with syntax-aware tokenization (directives, types, method names, string literals, etc.).
- Method cross-references resolve to the right class + line.

**UI**
- Tabbed right pane with overflow-safe dropdown, close buttons, click-to-activate.
- Horizontal + vertical scrollbars on listing, hex, and manifest views.
- Cmd-F symbol palette with fuzzy filter.
- Cmd-O open, Cmd-N new window. macOS app menu with **File → Open Recent** (last 10 bundles).
- Window bounds + open tabs + tree expansion state persisted per-bundle in `redb`; relaunching reopens where you left off.

### What's missing

- iOS / IPA loading (loader is stubbed).
- armv7 / x86 disassembly (non-AArch64 code sections currently route to the hex view).
- Renaming, commenting, persistent annotations on instructions.
- Cross-references DEX ↔ native via JNI signatures.
- QuickJS scripting host.
- Drag-to-scroll on scrollbars (currently visual-only — use trackpad / wheel).
- Resource ID decoding in the manifest (would need `resources.arsc` parsing).

## Building

Glass is currently **macOS-only** (the UI is built on `gpui` + Metal). Linux and Windows ports are possible but not yet on the near-term roadmap.

There are no pre-built binaries yet, so you'll need to build from source. The good news: it's two commands.

1. **Install Rust** (if you don't already have it):

   ```sh
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. **Clone and build**:

   ```sh
   git clone https://github.com/azw413/Glass.git
   cd glass
   cargo build --release -p glass-cli
   cp target/release/glass <to somewhere on your PATH>
   ```
   
   The first build will compile `gpui` and friends and will take several minutes. Subsequent builds are fast.
   
3. **Run it**:

   ```sh
   # Open the GUI on an APK
   ./target/release/glass gui ~/path/to/app.apk
   
   # Or on a single native binary
   ./target/release/glass gui ~/path/to/libfoo.so
   
   # Headless bundle inspect
   ./target/release/glass bundle ~/path/to/app.apk
   
   # Inspect persisted state for a bundle
   ./target/release/glass db-dump ~/path/to/app.apk
   ```

Always use the release build — debug builds disassemble orders of magnitude slower.

## Workspace

| Crate              | Purpose                                                       |
|--------------------|---------------------------------------------------------------|
| `glass-core`       | Shared types (`CodeKind`, IDs)                                |
| `glass-arch-arm64` | AArch64 disassembly, symbol map, PLT synthesis, demangling    |
| `glass-arch-dex`   | DEX / smali facade over `smali`                               |
| `glass-mobile`     | APK + IPA bundle loading, native-lib extraction, manifest     |
| `glass-db`         | Content-addressed persistence (redb): bundles, tabs, settings |
| `glass-ui`         | `gpui` front-end: tree, listing, hex, manifest, palette       |
| `glass-cli`        | Headless inspector + GUI launcher                             |
| `glass-script`     | QuickJS plugin runtime (placeholder)                          |

## Roadmap

- **Next** — Persistent comments and renames on instructions; cross-references between DEX call sites and JNI-bound native symbols.
- **iOS** — Thin-slice Mach-O for arm64e, Info.plist, embedded frameworks, ObjC `__objc_classlist`.
- **armv7** — 32-bit ARM disassembly for older `.so` variants.
- **Scripting** — QuickJS plugin host with a stable API for analysis passes.
- **Advanced** — Swift metadata, signed APK rebuilding, control-flow graph view.
