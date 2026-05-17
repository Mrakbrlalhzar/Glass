# Glass
*as in transparent and smooth*

A fast, native, **mobile-app first** interactive disassembler. Spiritual successor to IDA Pro for the Android / iOS reverse engineering workflow, built around:

- `smali` for APK / DEX / smali handling
- `armv8-encode` for AArch64 (native `.so`, iOS Mach-O)
- `gpui` (Zed) for GPU-accelerated native UI
- `redb` for content-addressed persistence
- `rquickjs` for scriptable plugins (planned)

License: GPL-3.0-only (inherited from `smali`).

## Why?

We’ve all used IDA Pro — it’s the industry standard for reversing and has years of plugins behind it, but it’s slow, expensive, and dated. Glass is 100% Rust native with a GPU-accelerated UI for fluid interaction. It’s also 100% free and open source — please contribute.

## Features
* Buttery smooth 120fps GPU accelerated rendering
* Lightning fast analysis: 1-2 seconds for most larger binaries compared with minutes on IDA Pro
* Fully linked and annotated disassemblies with control flow lines, data literals in comments, clickable links to other functions. All coloured for easy visibility.
* Control flow graphs showing basic blocks and clickable links to other functions
* Full project search for symbols or string literals across DEX, code and data sections
* Native binary layout overview with section data

## Status

Glass is usable today for reversing both Android (APK / DEX / native `.so`) and iOS (IPA / Mach-O) apps targeting AArch64. 32-bit ARM is on the roadmap.

### What works

**File loading**
- Open Android bundles (`.apk`, `.aab`), iOS bundles (`.ipa`), or any standalone ELF / Mach-O binary (`.so`, `.dylib`, raw executables) directly — Glass auto-detects the format.
- Fat / universal Mach-O is handled transparently: `arm64e` is preferred, plain `arm64` is the fallback. Works on bundles and on standalone files alike (e.g. `glass gui /usr/lib/dyld`).
- Loader pipeline reports progress (Reading archive → Parsing DEX / Disassembling native → Building symbols).
- Per-artifact content-addressed IDs (blake3, rayon-parallel for large libs). Annotations follow the artifact, not the container — the same `libfoo.so` shipped in two APKs (or the same `libswiftCore.dylib` across two IPAs) shares analysis state.
- AndroidManifest viewer (binary XML decoded via `smali`).
- Info.plist viewer for iOS bundles — bundle id, executable name, version, min OS, and the rest of the plist rendered as colour-coded XML.

**iOS — IPA / Mach-O**
- Unzip the IPA, locate `Payload/*.app/`, parse `Info.plist`, and pick the arm64 / arm64e slice from any fat binary inside.
- Main executable and every `Frameworks/*.framework` + `*.dylib` is loaded as its own native artifact, with the same Overview + per-section disassembly views used for Android `.so` files.

**Android — APK / DEX / native**
- Class tree across all DEX files in the APK.
- Smali listing per class with syntax-aware tokenization (directives, types, method names, string literals, etc.).
- Method cross-references resolve to the right class + line.
- Native `.so` files under `lib/<abi>/` loaded per ABI; AArch64 gets disassembly, other ABIs route to the hex view.

**AArch64 native (ELF + thin Mach-O)**
- Linear-sweep disassembly with virtualized rendering — large libraries open in seconds, not minutes.
- Symbol map merged from ELF symtab, dynsym, DWARF, `.eh_frame` FDEs, and synthesized `<name>@plt` entries. C++/Rust/Swift demangling via `symbolic-demangle`.
- Branch operands rendered as clickable symbol references; `adrp` + `add`/`ldr` pairs resolved to data targets, including string literals shown inline as comments.
- Per-section views (code sections get disassembly; data sections get a hex view).

**UI**
- Tabbed right pane with overflow-safe dropdown, close buttons, click-to-activate.
- Horizontal + vertical scrollbars on listing, hex, and manifest views.
- Cmd-F symbol palette with fuzzy filter.
- Cmd-O open, Cmd-N new window. macOS app menu with **File → Open Recent** (last 10 bundles).
- Window bounds + open tabs + tree expansion state persisted per-bundle in `redb`; relaunching reopens where you left off.

### What's missing

- armv7 / x86 disassembly (non-AArch64 code sections currently route to the hex view).
- iOS entitlements and `embedded.mobileprovision` parsing.
- Swift metadata pass — Swift Mach-O symbol stubs are sparse without it.
- ObjC `__objc_classlist` extraction.
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
   # Open the GUI on an Android APK or iOS IPA
   ./target/release/glass gui ~/path/to/app.apk
   ./target/release/glass gui ~/path/to/app.ipa

   # Or on a standalone binary — ELF .so, Mach-O .dylib, or raw
   # executable. Fat / universal Mach-O is sliced automatically.
   ./target/release/glass gui ~/path/to/libfoo.so
   ./target/release/glass gui ~/path/to/libBar.dylib
   ./target/release/glass gui /usr/lib/dyld

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
- **iOS deeper** — Entitlements, `embedded.mobileprovision`, ObjC `__objc_classlist`, Swift metadata pass.
- **armv7** — 32-bit ARM disassembly for older `.so` variants.
- **Scripting** — QuickJS plugin host with a stable API for analysis passes.
- **Advanced** — Signed APK rebuilding, control-flow graph view.
