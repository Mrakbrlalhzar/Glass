//! glass-ui: minimal GPUI shell.
//!
//! Single-file UI: window, two-pane layout, virtualized tree on the left,
//! pre-rendered body text on the right. Tree groups APK content as:
//!     classes.dex
//!       com.example.foo
//!         MainActivity
//!         Utils
//!     lib/arm64-v8a
//!       libfoo.so
//!
//! When this grows past ~600 lines or a hex view / command palette lands,
//! split into separate modules.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use glass_arch_arm64::Arm64Binary;
use glass_mobile::{ApkBundle, Bundle, IpaBundle};
use gpui::{
    App, Bounds, Context, FocusHandle, KeyBinding, ListAlignment, ListOffset, ListState, Pixels,
    Render, SharedString, Window, WindowBounds, WindowOptions, actions, div, list, prelude::*,
    px, rgb, size,
};
use gpui_platform::application;

const SCROLLBAR_WIDTH: f32 = 10.;
const SCROLLBAR_MIN_THUMB: f32 = 24.;

actions!(
    glass,
    [
        TogglePalette,
        PaletteClose,
        PaletteUp,
        PaletteDown,
        PaletteActivate,
        OpenFile,
        NewWindow,
        CloseWindow,
        Quit,
        // Up to 10 recent-bundle slots. Each is a zero-sized action
        // wired to a separate handler that opens index N from the
        // recent list. Avoids needing serde-deriving payload actions
        // (gpui supports them but requires schemars + JSON deser
        // setup).
        OpenRecent0,
        OpenRecent1,
        OpenRecent2,
        OpenRecent3,
        OpenRecent4,
        OpenRecent5,
        OpenRecent6,
        OpenRecent7,
        OpenRecent8,
        OpenRecent9,
    ]
);

const RECENT_SLOTS: usize = 10;

/// Launch the UI. If `path` is provided, opens the window immediately and
/// loads the bundle on a background task with live progress reporting.
/// If `fresh` is true, the persistence layer is bypassed on read (writes
/// still happen so the new session takes over once relaunched normally).
/// On macOS, the leftmost menu-bar item's title comes from the
/// process name, *not* from `cx.set_menus`. When the binary is named
/// `glass` (lowercase), the menu item reads `glass`. Override the
/// process name with NSProcessInfo so the menu reads "Glass".
#[cfg(target_os = "macos")]
fn set_macos_process_name(name: &str) {
    use objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let info_cls = class!(NSProcessInfo);
        let info: *mut objc::runtime::Object = msg_send![info_cls, processInfo];
        let str_cls = class!(NSString);
        let cstr = std::ffi::CString::new(name).unwrap();
        let ns_name: *mut objc::runtime::Object =
            msg_send![str_cls, stringWithUTF8String: cstr.as_ptr()];
        let _: () = msg_send![info, setProcessName: ns_name];
    }
}

#[cfg(not(target_os = "macos"))]
fn set_macos_process_name(_name: &str) {}

pub fn launch(path: Option<PathBuf>, fresh: bool) -> Result<()> {
    set_macos_process_name("Glass");
    let db = match glass_db::Database::open(fresh) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!("could not open glass-db: {e:#}; persistence disabled");
            None
        }
    };
    application().run(move |cx: &mut App| {
        cx.init_colors();
        cx.bind_keys([
            KeyBinding::new("cmd-f", TogglePalette, None),
            KeyBinding::new("escape", PaletteClose, None),
            KeyBinding::new("up", PaletteUp, None),
            KeyBinding::new("down", PaletteDown, None),
            KeyBinding::new("enter", PaletteActivate, None),
            KeyBinding::new("cmd-o", OpenFile, None),
            KeyBinding::new("cmd-n", NewWindow, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);

        // App-level action handlers — fire whether or not a window
        // has focus, matching native menu-bar expectations.
        {
            let db_for_open = db.clone();
            cx.on_action(move |_: &OpenFile, cx: &mut App| {
                open_file_dialog_and_window(db_for_open.clone(), cx);
            });
        }
        {
            let db_for_new = db.clone();
            cx.on_action(move |_: &NewWindow, cx: &mut App| {
                open_glass_window(None, db_for_new.clone(), cx);
            });
        }
        cx.on_action(|_: &Quit, cx: &mut App| {
            cx.quit();
        });

        register_open_recent_actions(db.clone(), cx);
        set_app_menus(cx, db.as_ref());

        open_glass_window(path.clone(), db.clone(), cx);
        cx.activate(true);
    });
    Ok(())
}

/// Register one action handler per recent-bundle slot. Each handler
/// reopens the bundle whose source path is at index N of the current
/// recent list. We snapshot the list lazily (inside the handler) so
/// it always reflects the latest DB state.
fn register_open_recent_actions(db: Option<glass_db::Database>, cx: &mut App) {
    macro_rules! wire {
        ($action:ty, $idx:expr) => {{
            let db = db.clone();
            cx.on_action(move |_: &$action, cx: &mut App| {
                open_nth_recent(db.clone(), $idx, cx);
            });
        }};
    }
    wire!(OpenRecent0, 0);
    wire!(OpenRecent1, 1);
    wire!(OpenRecent2, 2);
    wire!(OpenRecent3, 3);
    wire!(OpenRecent4, 4);
    wire!(OpenRecent5, 5);
    wire!(OpenRecent6, 6);
    wire!(OpenRecent7, 7);
    wire!(OpenRecent8, 8);
    wire!(OpenRecent9, 9);
}

fn open_nth_recent(db: Option<glass_db::Database>, idx: usize, cx: &mut App) {
    let Some(handle) = db.clone() else { return };
    let recents = handle.recent_bundles(RECENT_SLOTS);
    let Some(rec) = recents.into_iter().nth(idx) else { return };
    let Some(path) = rec.source_path else { return };
    open_glass_window(Some(PathBuf::from(path)), db, cx);
}

fn set_app_menus(cx: &mut App, db: Option<&glass_db::Database>) {
    // Pre-resolve the recent list so menu items get real labels.
    let recents: Vec<glass_db::BundleRecord> = db
        .map(|d| d.recent_bundles(RECENT_SLOTS))
        .unwrap_or_default();

    let recent_items: Vec<gpui::MenuItem> = if recents.is_empty() {
        vec![gpui::MenuItem::action("No recent files", OpenRecent0).disabled(true)]
    } else {
        recents
            .iter()
            .enumerate()
            .take(RECENT_SLOTS)
            .map(|(i, rec)| {
                let label = rec.label.clone();
                // gpui's MenuItem::action wants `impl Action` (a
                // concrete type), so we match on the slot index and
                // pick the matching zero-sized action type.
                match i {
                    0 => gpui::MenuItem::action(label, OpenRecent0),
                    1 => gpui::MenuItem::action(label, OpenRecent1),
                    2 => gpui::MenuItem::action(label, OpenRecent2),
                    3 => gpui::MenuItem::action(label, OpenRecent3),
                    4 => gpui::MenuItem::action(label, OpenRecent4),
                    5 => gpui::MenuItem::action(label, OpenRecent5),
                    6 => gpui::MenuItem::action(label, OpenRecent6),
                    7 => gpui::MenuItem::action(label, OpenRecent7),
                    8 => gpui::MenuItem::action(label, OpenRecent8),
                    _ => gpui::MenuItem::action(label, OpenRecent9),
                }
            })
            .collect()
    };

    cx.set_menus([
        gpui::Menu::new("Glass").items([
            gpui::MenuItem::action("About Glass", Quit).disabled(true),
            gpui::MenuItem::separator(),
            gpui::MenuItem::action("Quit", Quit),
        ]),
        gpui::Menu::new("File").items({
            let mut items: Vec<gpui::MenuItem> = vec![
                gpui::MenuItem::action("Open…", OpenFile),
                gpui::MenuItem::submenu(
                    gpui::Menu::new("Open Recent").items(recent_items),
                ),
                gpui::MenuItem::separator(),
                gpui::MenuItem::action("New Window", NewWindow),
                gpui::MenuItem::action("Close Window", CloseWindow),
            ];
            items.shrink_to_fit();
            items
        }),
        gpui::Menu::new("View").items([
            gpui::MenuItem::action("Search…", TogglePalette),
        ]),
        gpui::Menu::new("Window").items([
            // Minimal — gpui adds standard items here on macOS.
        ]),
    ]);
}

fn open_file_dialog_and_window(
    db: Option<glass_db::Database>,
    cx: &mut App,
) {
    let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(gpui::SharedString::from("Open APK / IPA / binary")),
    });
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = receiver.await else { return };
        let Some(path) = paths.into_iter().next() else { return };
        let _ = cx.update(|cx| open_glass_window(Some(path), db, cx));
    })
    .detach();
}

/// Open a Glass window with the user's last-known size (centered fall
/// back when there isn't one). Persists size on resize/move so the
/// next launch reopens at the same bounds.
fn open_glass_window(
    path: Option<PathBuf>,
    db: Option<glass_db::Database>,
    cx: &mut App,
) {
    let settings = glass_db::load_window_settings();
    let bounds = match settings.bounds {
        Some(b) => Bounds {
            origin: gpui::point(px(b.x), px(b.y)),
            size: size(px(b.width), px(b.height)),
        },
        None => Bounds::centered(None, size(px(1200.), px(800.)), cx),
    };
    let path_for_window = path.clone();
    let db_for_window = db.clone();
    cx.open_window(
        WindowOptions {
            focus: true,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| {
            let shell = cx.new(|cx| {
                Shell::new(path_for_window.clone(), db_for_window.clone(), window, cx)
            });
            if let Some(p) = path_for_window.clone() {
                spawn_loader(&shell, p, cx);
            }
            if let Some(db) = db_for_window.clone() {
                spawn_flush_timer(&shell, db, cx);
            }
            // Persist window bounds on every resize/move. The
            // observer fires synchronously during drag — cheap so no
            // debounce needed; the file write is a few-byte JSON.
            shell.update(cx, |_shell, cx| {
                cx.observe_window_bounds(window, |_shell, window, _cx| {
                    let b = window.bounds();
                    let _ = glass_db::save_window_settings(&glass_db::WindowSettings {
                        bounds: Some(glass_db::StoredBounds {
                            x: b.origin.x.as_f32(),
                            y: b.origin.y.as_f32(),
                            width: b.size.width.as_f32(),
                            height: b.size.height.as_f32(),
                        }),
                    });
                })
                .detach();
            });
            shell
        },
    )
    .expect("open_window");
}

/// Drive `db.flush()` every 500ms while the window lives. Cheap when
/// nothing is dirty.
fn spawn_flush_timer(shell: &gpui::Entity<Shell>, db: glass_db::Database, cx: &mut App) {
    let interval = db.flush_interval();
    let weak = shell.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(interval).await;
            // Stop if the shell has been dropped (window closed).
            if weak.upgrade().is_none() {
                if let Err(e) = db.flush() {
                    tracing::warn!("glass-db final flush failed: {e:#}");
                }
                break;
            }
            if let Err(e) = db.flush() {
                tracing::warn!("glass-db flush failed: {e:#}");
            }
        }
    })
    .detach();
}

fn spawn_loader(shell: &gpui::Entity<Shell>, path: PathBuf, cx: &mut App) {
    let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::starting(&path)));
    shell.update(cx, |s, _| s.progress = Some(progress.clone()));

    // Hand the blocking load off to a worker thread. `gpui::BackgroundExecutor`
    // requires `Send` futures, so the body can't touch the gpui context.
    let bg_progress = progress.clone();
    let loader_task = cx.background_executor().spawn(async move {
        load_bundle_blocking(path, bg_progress)
    });

    // Foreground poll loop: tick the UI ~30fps while waiting for the loader.
    // Polling here keeps `AsyncApp` (which is !Send) on the right thread.
    let weak = shell.downgrade();
    let progress_for_poll = progress.clone();
    cx.spawn(async move |cx| {
        // Race the loader against the timer-driven poll.
        let mut loader = Some(loader_task);
        loop {
            // First, check if the loader is done by polling it via a quick
            // race: sleep a short bit, then test. If done, take the result.
            cx.background_executor()
                .timer(Duration::from_millis(33))
                .await;

            let _ = weak.update(cx, |_s, cx| cx.notify());

            // Look at the shared progress to know if the loader finished.
            let done = progress_for_poll.lock().map(|p| p.done).unwrap_or(true);
            if done {
                break;
            }
        }

        // Await the final result (immediate now that `done` is set).
        let result = loader.take().expect("loader task").await;

        let _ = weak.update(cx, |shell, cx| {
            match result {
                Ok(bundle) => shell.state = ShellState::Ready(bundle),
                Err(e) => shell.state = ShellState::Error(format!("{e:#}")),
            }
            shell.progress = None;
            if let ShellState::Ready(b) = &shell.state {
                // Default-expand every top-level group.
                for (i, _) in b.tree.roots.iter().enumerate() {
                    shell.expanded.open.insert(vec![i]);
                }
                // Then let persisted state override expansion and re-open
                // any tabs the user had last time.
                let bundle = b.clone();
                shell.restore_state(&bundle);
                shell.rebuild_list_state();
                // Persist a fresh BundleRecord (updates last_opened_unix
                // even if nothing else changed).
                shell.save_state();
            }
            cx.notify();
        });
    })
    .detach();
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub label: String,
    pub phase: SharedString,
    pub current: usize,
    pub total: usize,
    pub done: bool,
}

impl Progress {
    fn starting(path: &std::path::Path) -> Self {
        Self {
            label: path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("(bundle)")
                .to_string(),
            phase: SharedString::from("Opening…"),
            current: 0,
            total: 0,
            done: false,
        }
    }
}

enum ShellState {
    Empty,
    Loading,
    Ready(LoadedBundle),
    Error(String),
}

fn load_bundle_blocking(path: PathBuf, progress: Arc<Mutex<Progress>>) -> Result<LoadedBundle> {
    let result = load_inner(&path, &progress);
    // Make sure the foreground poll loop notices we're done even on error.
    if let Ok(mut p) = progress.lock() {
        p.done = true;
    }
    result
}

fn load_inner(path: &std::path::Path, progress: &Arc<Mutex<Progress>>) -> Result<LoadedBundle> {
    if let Ok(mut p) = progress.lock() {
        p.phase = SharedString::from("Reading archive…");
        p.current = 0;
        p.total = 0;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if matches!(ext, "apk" | "aab") {
        // Open the APK first so we have access to its DEX and native-
        // lib bytes. We used to also `fs::read` the whole APK file to
        // hash it — but that's a 350 MB+ read on big games. The
        // BundleId is derived from the concatenated ArtifactIds below
        // instead: same content-addressed guarantee, no extra I/O.
        if let Ok(mut p) = progress.lock() {
            p.phase = SharedString::from("Reading archive…");
        }
        let apk = match glass_mobile::Bundle::open(path)? {
            Bundle::Apk(a) => a,
            _ => anyhow::bail!("expected APK"),
        };
        let display_label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bundle")
            .to_string();
        snapshot_apk_with_progress(apk, progress.clone(), display_label)
    } else if matches!(ext, "ipa") {
        if let Ok(mut p) = progress.lock() {
            p.phase = SharedString::from("Reading archive…");
        }
        let ipa = match glass_mobile::Bundle::open(path)? {
            Bundle::Ipa(i) => i,
            _ => anyhow::bail!("expected IPA"),
        };
        let display_label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bundle")
            .to_string();
        snapshot_ipa_with_progress(ipa, progress.clone(), display_label)
    } else {
        // Standalone binary: ELF (`.so`, no-ext executables) or Mach-O
        // (`.dylib`, no-ext executables — possibly fat). Arm64Binary
        // transparently slices fat Mach-Os down to arm64/arm64e.
        let bin = Arm64Binary::open(path)?;
        snapshot_arm64(bin)
    }
}

// ---- snapshots --------------------------------------------------------------

#[derive(Clone)]
pub struct LoadedBundle {
    pub title: String,
    pub tree: Arc<Tree>,
    /// Pre-rendered bodies, keyed by `LeafId`.
    pub bodies: Arc<Vec<SharedString>>,
    /// Subtitle for each leaf (e.g. "classes.dex" or "lib/arm64-v8a").
    pub origins: Arc<Vec<SharedString>>,
    /// Short label for each leaf — used as the tab title. For DEX classes
    /// we keep just the simple name (`Foo` from `Lcom/example/Foo;`).
    pub labels: Arc<Vec<SharedString>>,
    /// What kind of view each leaf opens. Parallel to `bodies` etc.
    pub kinds: Arc<Vec<LeafKind>>,
    /// blake3 of the source bytes — the persistence key. `None` for the
    /// standalone arm64 case until that grows real artifact identity.
    pub bundle_id: Option<glass_db::BundleId>,
    /// Artifact hashes parallel to whatever the snapshot considers an
    /// artifact: each DEX, each native lib. Indices are private to the
    /// snapshot — persistence stores the whole list in the BundleRecord.
    pub artifact_ids: Arc<Vec<glass_db::ArtifactId>>,
    /// Display label for the bundle in the title bar (just the filename).
    pub display_label: String,
    /// Per-native-artifact section info, keyed by ArtifactId.
    /// Empty for DEX-only artifacts.
    pub native_sections: Arc<std::collections::HashMap<glass_db::ArtifactId, Vec<SectionInfo>>>,
    /// Per-native-artifact merged symbol map (symtab + DWARF + .eh_frame).
    pub symbol_maps: Arc<std::collections::HashMap<glass_db::ArtifactId, glass_arch_arm64::SymbolMap>>,
    /// Text sections we can disassemble on demand. One entry per
    /// `SectionKind::Text` section per native artifact. Keyed by
    /// `(artifact, section_name)` so the Listing tab can look up by
    /// the same `(artifact, section)` it already carries.
    pub text_sections: Arc<std::collections::HashMap<(glass_db::ArtifactId, String), TextSectionBytes>>,
    /// Non-text section bytes (data / rodata / plt / etc.) for the hex
    /// view. Same `(artifact, section_name)` keying as `text_sections`.
    pub data_sections: Arc<std::collections::HashMap<(glass_db::ArtifactId, String), DataSectionBytes>>,
    /// Smali method-reference → location map. Keyed by the full
    /// `Class;->name(sig)ret` form (as it appears in source), valued
    /// with `(leaf_id, line_index)` — the SmaliClass leaf and the
    /// 0-based line within its body where the `.method` declaration
    /// starts. Built once at load, used by the smali renderer for
    /// method-ref deep links.
    pub method_lines: Arc<std::collections::HashMap<String, (LeafId, usize)>>,
    /// Pre-flattened AndroidManifest rows for the XML viewer. Empty
    /// for non-APK bundles or APKs without a parseable manifest.
    pub manifest_rows: Arc<Vec<ManifestRow>>,
}

/// Owned bytes + base address for a text section. Cheap to clone via Arc.
#[derive(Clone)]
pub struct TextSectionBytes {
    pub base: u64,
    pub bytes: Arc<Vec<u8>>,
}

/// Owned bytes + base address for a non-text section, used by the hex
/// view. We could fold this into a single SectionBytes type, but
/// keeping them separate makes the "code vs data" distinction explicit
/// at call sites that only want one or the other.
#[derive(Clone)]
pub struct DataSectionBytes {
    pub base: u64,
    pub bytes: Arc<Vec<u8>>,
    pub kind: NativeSectionKind,
}

impl DataSectionBytes {
    /// How many 16-byte rows the hex view will render.
    pub fn row_count(&self) -> usize {
        (self.bytes.len() + 15) / 16
    }

    /// Base address of the `n`-th row.
    pub fn row_addr(&self, row: usize) -> u64 {
        self.base + (row as u64) * 16
    }

    /// Row that contains `addr`, clamped to range.
    pub fn row_of(&self, addr: u64) -> usize {
        let off = addr.saturating_sub(self.base) as usize;
        (off / 16).min(self.row_count().saturating_sub(1))
    }

    /// Slice of bytes for the given row (1..=16 long).
    pub fn row_bytes(&self, row: usize) -> &[u8] {
        let start = row * 16;
        let end = (start + 16).min(self.bytes.len());
        &self.bytes[start..end]
    }
}

impl TextSectionBytes {
    pub fn instruction_count(&self) -> usize {
        self.bytes.len() / 4
    }

    pub fn addr_of(&self, index: usize) -> u64 {
        self.base + (index as u64) * 4
    }

    pub fn index_of(&self, addr: u64) -> usize {
        let off = addr.saturating_sub(self.base) as usize;
        (off / 4).min(self.instruction_count().saturating_sub(1))
    }

    pub fn word_at(&self, index: usize) -> Option<(u64, [u8; 4], u32)> {
        let off = index * 4;
        if off + 4 > self.bytes.len() {
            return None;
        }
        let chunk = &self.bytes[off..off + 4];
        let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
        Some((self.addr_of(index), bytes, u32::from_le_bytes(bytes)))
    }
}

/// One precomputed entry in a Listing tab's row list.
#[derive(Clone, Debug)]
pub enum ListingRow {
    /// `<symbol>:` line preceding a symbol entry point.
    SymbolHeader { name: SharedString },
    /// One AArch64 instruction.
    Instruction {
        address: u64,
        bytes: [u8; 4],
        mnemonic: SharedString,
        operands: Arc<Vec<glass_arch_arm64::Chunk>>,
        /// Trailing `; ...` comment chunks. Empty if no annotation.
        comment: SharedString,
    },
    /// Horizontal rule drawn after a basic-block terminator.
    BasicBlockSeparator,
}

/// One precomputed row in a Hex tab's row list.
#[derive(Clone, Debug)]
pub enum HexRow {
    SymbolHeader { name: SharedString },
    Bytes {
        /// Absolute address of the first byte in the row.
        address: u64,
        /// Up to 16 bytes from the section. Length may be less than 16
        /// for the final row if the section size isn't 16-aligned.
        bytes: Vec<u8>,
    },
}

/// Walk a non-text section emitting hex rows interleaved with symbol
/// headers. Same shape as `build_listing_rows`, just one row per 16
/// bytes instead of per instruction.
pub fn build_hex_rows(
    data: &DataSectionBytes,
    symbols: &glass_arch_arm64::SymbolMap,
) -> Vec<HexRow> {
    let n = data.row_count();
    let mut rows = Vec::with_capacity(n + n / 16);

    for row_ix in 0..n {
        let addr = data.row_addr(row_ix);
        let row_bytes = data.row_bytes(row_ix);
        let row_end = addr + row_bytes.len() as u64;

        // Emit a symbol header for any symbol whose entry point lies
        // in this row (including its first byte). Symbols are sorted
        // by address, so collecting via `in_range` keeps insertion
        // order stable.
        for sym in symbols.in_range(addr, row_end) {
            rows.push(HexRow::SymbolHeader {
                name: SharedString::from(sym.display_name.clone()),
            });
        }

        rows.push(HexRow::Bytes {
            address: addr,
            bytes: row_bytes.to_vec(),
        });
    }

    rows
}

/// Walk a section's bytes, emitting rows: a symbol header at each
/// symbol entry, an instruction per word, a basic-block separator
/// after each control-flow terminator. Branch targets and ADRP
/// destinations are resolved to symbol names in the comment when
/// possible.
///
/// `progress`, if provided, is updated every 1024 instructions so a
/// progress bar UI can animate while a background thread runs the build.
/// Owning snapshot of an artifact's non-text bytes, used by
/// `build_listing_rows` to resolve ADRP+ADD targets to string
/// literals. The bytes are shared via `Arc` so passing this to a
/// worker thread is cheap.
pub struct DataPeek {
    pub sections: Vec<(u64, Arc<Vec<u8>>)>, // (base, bytes)
}

impl DataPeek {
    pub fn empty() -> Self {
        Self { sections: Vec::new() }
    }

    /// Best-effort ASCII string peek starting at `addr`. Returns up to
    /// `max_len` printable characters, terminated by a NUL or the
    /// first non-printable byte. `None` if `addr` doesn't fall in any
    /// known section, or the first byte isn't a printable ASCII.
    pub fn peek_string(&self, addr: u64, max_len: usize) -> Option<String> {
        // Walk every section that covers `addr` and return the first
        // that yields a valid printable run. Sections sometimes
        // overlap (especially when an artifact carries debug-info
        // copies of real data), so we can't short-circuit after the
        // first containing section without missing valid strings in
        // a different section that also contains the same address.
        for (base, bytes) in &self.sections {
            if addr < *base || addr >= base + bytes.len() as u64 {
                continue;
            }
            let off = (addr - base) as usize;
            let slice = &bytes[off..];
            if !slice.first().is_some_and(|b| (0x20..=0x7e).contains(b)) {
                continue;
            }
            let mut out = String::new();
            let mut ok = true;
            for &b in slice.iter().take(max_len) {
                if b == 0 {
                    break;
                }
                if !(0x20..=0x7e).contains(&b) {
                    ok = false;
                    break;
                }
                out.push(b as char);
            }
            if ok && out.len() >= 2 {
                return Some(out);
            }
        }
        None
    }
}

/// X-register indices in the decoded operands, in order they appear.
/// SP shares an index space with the GP registers via RegisterClass,
/// but ADRP/ADD targets are always GP X-registers in practice.
fn x_regs_of(insn: &armv8_encode::isa::aarch64::DecodedInstruction) -> Vec<u8> {
    use armv8_encode::isa::aarch64::{DecodedOperand, RegisterClass};
    let mut out = Vec::with_capacity(insn.operands.len());
    for op in &insn.operands {
        if let DecodedOperand::Register(r) = op {
            if matches!(r.class, RegisterClass::X | RegisterClass::XOrSp) {
                out.push(r.index);
            }
        }
    }
    out
}

/// Pull an immediate value out of an instruction's operands. Supports
/// plain Immediate, UnsignedImmediate and ShiftedImmediate. None if
/// there's no immediate operand.
fn first_imm_of(insn: &armv8_encode::isa::aarch64::DecodedInstruction) -> Option<i64> {
    use armv8_encode::isa::aarch64::DecodedOperand;
    for op in &insn.operands {
        match op {
            DecodedOperand::Immediate(v) => return Some(*v),
            DecodedOperand::UnsignedImmediate(v) => return Some(*v as i64),
            DecodedOperand::ShiftedImmediate(s) => {
                return Some(s.value.wrapping_shl(s.shift as u32))
            }
            _ => {}
        }
    }
    None
}

/// If `insn` is `adrp Xd, target`, return `(d_index, target)`.
fn extract_adrp(
    insn: &armv8_encode::isa::aarch64::DecodedInstruction,
) -> Option<(u8, u64)> {
    use armv8_encode::isa::aarch64::{Aarch64Mnemonic, DecodedOperand};
    if insn.mnemonic != Aarch64Mnemonic::Adrp {
        return None;
    }
    let regs = x_regs_of(insn);
    let page = insn.operands.iter().find_map(|op| match op {
        DecodedOperand::PageTarget(a) => Some(*a),
        _ => None,
    });
    Some((*regs.first()?, page?))
}

/// If `insn` is an `add Xd, Xs, #imm` whose `Xs` has a known page base,
/// return `(d_index, s_index, final_addr)`. Returns `None` for any add
/// shape that isn't a simple `Xd <- Xs + immediate`.
fn extract_add_with_imm(
    insn: &armv8_encode::isa::aarch64::DecodedInstruction,
    page_bases: &[Option<u64>; 32],
) -> Option<(u8, u8, u64)> {
    use armv8_encode::isa::aarch64::Aarch64Mnemonic;
    if insn.mnemonic != Aarch64Mnemonic::Add {
        return None;
    }
    let regs = x_regs_of(insn);
    if regs.len() < 2 {
        return None;
    }
    let d = regs[0];
    let s = regs[1];
    let base = page_bases.get(s as usize).copied().flatten()?;
    let imm = first_imm_of(insn)?;
    if imm < 0 {
        return None;
    }
    Some((d, s, base.wrapping_add(imm as u64)))
}

/// Index of the X-register written by `insn`, if any. Used to
/// invalidate stale page bases when the destination gets clobbered by
/// a later instruction. Conservative — we treat the first X-register
/// operand as the destination, which is correct for almost every
/// ARM64 instruction we care about (data-proc, ldr, mov, …).
fn dest_x_reg(insn: &armv8_encode::isa::aarch64::DecodedInstruction) -> Option<u8> {
    x_regs_of(insn).into_iter().next()
}

// ---- search index -----------------------------------------------------------
//
// Lazily built by the loader on a background task. Flat `Vec<SearchEntry>`
// scanned linearly per keystroke — fine up to ~200k entries which is what
// a big native lib + DEX yields.

#[derive(Clone, Debug)]
pub enum SearchJump {
    Listing { artifact: glass_db::ArtifactId, section: String, addr: u64 },
    Hex { artifact: glass_db::ArtifactId, section: String, addr: u64 },
    SmaliClass { class_jni: String },
    SectionMap { artifact: glass_db::ArtifactId },
}

#[derive(Clone, Debug)]
pub struct SearchEntry {
    /// Primary display string we match against.
    pub display: String,
    /// Right-side chip (e.g. ".text · libfoo.so" or "method · com.example.Foo").
    pub chip: String,
    /// Single-character kind glyph for the left column.
    pub kind_glyph: &'static str,
    pub jump: SearchJump,
}

#[derive(Default)]
pub struct SearchIndex {
    pub entries: Vec<SearchEntry>,
}

impl SearchIndex {
    /// Filter the index against a query and return up to `cap` results,
    /// ranked: prefix-match > substring > char-subsequence, then by
    /// display length (shorter = closer).
    pub fn filter(&self, query: &str, cap: usize) -> Vec<&SearchEntry> {
        if query.is_empty() {
            // No query: don't bombard the user with arbitrary entries.
            // Wait until they actually type.
            return Vec::new();
        }
        let q = query.to_lowercase();
        let mut scored: Vec<(u8, usize, &SearchEntry)> = Vec::new();
        for e in &self.entries {
            let hay = e.display.to_lowercase();
            let tier = if hay.starts_with(&q) {
                0
            } else if hay.contains(&q) {
                1
            } else if is_subsequence(&q, &hay) {
                2
            } else {
                continue;
            };
            scored.push((tier, e.display.len(), e));
        }
        scored.sort_by_key(|&(tier, len, _)| (tier, len));
        scored.into_iter().take(cap).map(|(_, _, e)| e).collect()
    }
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut h = hay.chars();
    'outer: for nc in needle.chars() {
        for hc in h.by_ref() {
            if hc == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

pub fn build_listing_rows(
    text: &TextSectionBytes,
    symbols: &glass_arch_arm64::SymbolMap,
    data: &DataPeek,
    progress: Option<&Arc<Mutex<Progress>>>,
) -> Vec<ListingRow> {
    use glass_arch_arm64::format as fmt;
    let n = text.instruction_count();
    if let Some(p) = progress {
        if let Ok(mut p) = p.lock() {
            p.phase = SharedString::from("Disassembling…");
            p.current = 0;
            p.total = n;
        }
    }
    // Rough capacity: ~1.2 rows per insn (some symbol headers + BB
    // separators). Avoids most reallocations on large sections.
    let mut rows = Vec::with_capacity(n + n / 8);

    // ADRP+ADD pair tracking. For each X-register we remember the
    // most recent ADRP page address loaded into it; any later ADD that
    // sources from that register resolves to `page + imm`. We
    // invalidate a slot whenever an instruction writes to its
    // register (the conservative rule — a write loses the page base
    // for further resolution).
    let mut x_page_bases: [Option<u64>; 32] = [None; 32];

    for i in 0..n {
        if i % 1024 == 0 {
            if let Some(p) = progress {
                if let Ok(mut p) = p.lock() {
                    p.current = i;
                }
            }
        }
        let Some((addr, bytes, word)) = text.word_at(i) else { break };

        // Symbol header — if this address starts a named symbol.
        if let Some(sym) = symbols.at(addr) {
            rows.push(ListingRow::SymbolHeader {
                name: SharedString::from(sym.display_name.clone()),
            });
        }

        // Decode + format.
        let decoded = armv8_encode::isa::aarch64::decode_instruction(addr, word).ok();
        let (mnemonic, mut operands, terminates, target_addr) = match &decoded {
            Some(insn) => {
                let m = fmt::mnemonic_chunk(insn).text;
                let ops = fmt::operands_chunks(insn);
                let term = fmt::is_terminator(insn.mnemonic);
                let tgt = fmt::primary_address_operand(insn);
                (m, ops, term, tgt)
            }
            None => (
                ".word".to_string(),
                vec![glass_arch_arm64::Chunk {
                    text: format!("0x{word:08x}"),
                    kind: glass_arch_arm64::ChunkKind::Immediate,
                    target: None,
                    target_text: None,
                }],
                false,
                None,
            ),
        };

        // Resolve any Address chunks (branch/page targets) to symbol
        // names in-place. If the operand itself now names the target,
        // we don't need a trailing `;` comment.
        let mut named_in_operand = false;
        for op in &mut operands {
            if op.kind == glass_arch_arm64::ChunkKind::Address {
                if let Some(t) = op.target {
                    if let Some(sym) = symbols.covering(t) {
                        let off = t - sym.address;
                        op.text = if off == 0 {
                            sym.display_name.clone()
                        } else {
                            format!("{}+0x{off:x}", sym.display_name)
                        };
                        named_in_operand = true;
                    }
                }
            }
        }

        // Comment only when the operand itself doesn't name the target.
        let comment = if named_in_operand {
            SharedString::from("")
        } else {
            match target_addr.and_then(|a| symbols.covering(a)) {
                Some(s) => {
                    let off = target_addr.unwrap() - s.address;
                    if off == 0 {
                        SharedString::from(format!("; {}", s.display_name))
                    } else {
                        SharedString::from(format!("; {} + 0x{off:x}", s.display_name))
                    }
                }
                None => SharedString::from(""),
            }
        };

        // Pair / direct-address comment. Cases (first match wins):
        //   1. ADD Xd, Xs, #imm  where x_page_bases[Xs] is some(page)
        //      → resolved = page + imm; peek string.
        //   2. ADR Xd, label     → resolved = label; peek string.
        let mut resolved_addr: Option<u64> = None;
        if let Some(insn) = decoded.as_ref() {
            if let Some((_d, _s, target)) = extract_add_with_imm(insn, &x_page_bases) {
                resolved_addr = Some(target);
            } else if matches!(
                insn.mnemonic,
                armv8_encode::isa::aarch64::Aarch64Mnemonic::Adr
            ) {
                resolved_addr = insn.operands.iter().find_map(|op| match op {
                    armv8_encode::isa::aarch64::DecodedOperand::BranchTarget(a) => {
                        Some(*a)
                    }
                    _ => None,
                });
            }
        }
        let comment = if let Some(addr_for_string) = resolved_addr {
            match data.peek_string(addr_for_string, 64) {
                Some(s) => {
                    let trimmed: String = s.chars().take(64).collect();
                    SharedString::from(format!("; \"{trimmed}\""))
                }
                None => {
                    // Useful while debugging: tell us when we resolved
                    // an adrp/adr target but the bytes there weren't a
                    // printable string. Indicates either a different
                    // pattern (adrp+ldr) or a non-string pointer.
                    tracing::trace!(
                        "adrp/adr resolved to 0x{addr_for_string:x} \
                         (no printable string at target; \
                         data sections cached: {})",
                        data.sections.len()
                    );
                    comment
                }
            }
        } else {
            comment
        };

        rows.push(ListingRow::Instruction {
            address: addr,
            bytes,
            mnemonic: SharedString::from(mnemonic),
            operands: Arc::new(operands),
            comment,
        });

        // Update per-register page-base state.
        //
        //   - ADRP Xd, page  → x_page_bases[d] = page.
        //   - Otherwise, if the instruction writes Xd, invalidate
        //     x_page_bases[d] (a write loses the page base).
        if let Some(insn) = decoded.as_ref() {
            if let Some((d, page)) = extract_adrp(insn) {
                if (d as usize) < x_page_bases.len() {
                    x_page_bases[d as usize] = Some(page);
                }
            } else if let Some(d) = dest_x_reg(insn) {
                if (d as usize) < x_page_bases.len() {
                    x_page_bases[d as usize] = None;
                }
            }
        }

        if terminates {
            rows.push(ListingRow::BasicBlockSeparator);
        }
    }

    if let Some(p) = progress {
        if let Ok(mut p) = p.lock() {
            p.current = n;
            p.done = true;
        }
    }
    rows
}

/// Scroll a list so `target_row` sits roughly 10% down the viewport.
/// Leaves room above for the preceding symbol header / last few rows of
/// the previous function. Falls back to ~5 rows of context when the
/// viewport size isn't known yet (first paint).
fn scroll_into_view_with_context(state: &ListState, target_row: usize) {
    let viewport_h = state.viewport_bounds().size.height;
    let row_h = px(LISTING_ROW_HEIGHT);
    let context_rows = if viewport_h > px(0.) {
        let visible = (viewport_h / row_h) as usize;
        (visible / 10).max(3)
    } else {
        5
    };
    let top = target_row.saturating_sub(context_rows);
    state.scroll_to(ListOffset {
        item_ix: top,
        offset_in_item: px(0.),
    });
}

/// Find the hex row index containing `addr`, or the nearest one below.
// ---- AndroidManifest XML viewer --------------------------------------------

/// One pre-rendered row of the manifest viewer. We flatten the tree
/// into row-per-line up front so the virtualized list can render
/// without recursing per-frame.
#[derive(Clone, Debug)]
pub struct ManifestRow {
    /// Tree depth — used for indentation.
    pub depth: usize,
    /// Coloured tokens for this line.
    pub chunks: Arc<Vec<glass_arch_arm64::Chunk>>,
}

/// Flatten a parsed AndroidManifest into one Vec<ManifestRow>. The
/// XML rendering follows the usual indented form:
///   <manifest android:foo="bar"
///             android:baz="qux">
///     <application ...>
///       <activity .../>
///     </application>
///   </manifest>
pub fn flatten_manifest(
    manifest: &smali::android::binary_xml::AndroidManifest,
) -> Vec<ManifestRow> {
    let mut rows = Vec::new();
    flatten_element(manifest.root(), 0, &mut rows);
    rows
}

fn flatten_element(
    elem: &smali::android::binary_xml::ManifestElement,
    depth: usize,
    rows: &mut Vec<ManifestRow>,
) {
    use glass_arch_arm64::{Chunk, ChunkKind};

    let mk =
        |text: String, kind: ChunkKind| Chunk { text, kind, target: None, target_text: None };

    // Element open: `<tag` plus attributes (each attribute on its own
    // continuation line when there are 2+, otherwise inline).
    let tag = qualified_tag(elem);
    let self_closing = elem.children.is_empty() && elem.text.is_none();

    if elem.attributes.is_empty() {
        let mut chunks = vec![mk("<".to_string(), ChunkKind::Punct)];
        chunks.push(mk(tag.clone(), ChunkKind::Directive));
        chunks.push(mk(if self_closing { "/>".to_string() } else { ">".to_string() }, ChunkKind::Punct));
        rows.push(ManifestRow { depth, chunks: Arc::new(chunks) });
    } else if elem.attributes.len() == 1 {
        let mut chunks = vec![mk("<".to_string(), ChunkKind::Punct)];
        chunks.push(mk(tag.clone(), ChunkKind::Directive));
        chunks.push(mk(" ".to_string(), ChunkKind::Plain));
        push_attribute(&elem.attributes[0], &mut chunks);
        chunks.push(mk(if self_closing { "/>".to_string() } else { ">".to_string() }, ChunkKind::Punct));
        rows.push(ManifestRow { depth, chunks: Arc::new(chunks) });
    } else {
        // First line carries the open `<tag` and first attribute.
        let mut first = vec![mk("<".to_string(), ChunkKind::Punct)];
        first.push(mk(tag.clone(), ChunkKind::Directive));
        first.push(mk(" ".to_string(), ChunkKind::Plain));
        push_attribute(&elem.attributes[0], &mut first);
        rows.push(ManifestRow { depth, chunks: Arc::new(first) });
        // Subsequent attributes get their own continuation line at
        // `depth + 1` so they line up under the tag's first attribute.
        for (i, attr) in elem.attributes.iter().enumerate().skip(1) {
            let last = i == elem.attributes.len() - 1;
            let mut chunks = Vec::new();
            push_attribute(attr, &mut chunks);
            if last {
                chunks.push(mk(
                    if self_closing { "/>".to_string() } else { ">".to_string() },
                    ChunkKind::Punct,
                ));
            }
            rows.push(ManifestRow { depth: depth + 1, chunks: Arc::new(chunks) });
        }
    }

    // Inline text content — uncommon for manifest but supported.
    if let Some(text) = elem.text.as_deref() {
        if !text.trim().is_empty() {
            let chunks = vec![mk(text.to_string(), ChunkKind::String)];
            rows.push(ManifestRow { depth: depth + 1, chunks: Arc::new(chunks) });
        }
    }

    // Children, then close tag.
    for child in &elem.children {
        flatten_element(child, depth + 1, rows);
    }
    if !self_closing {
        let mut chunks = vec![mk("</".to_string(), ChunkKind::Punct)];
        chunks.push(mk(tag, ChunkKind::Directive));
        chunks.push(mk(">".to_string(), ChunkKind::Punct));
        rows.push(ManifestRow { depth, chunks: Arc::new(chunks) });
    }
}

fn qualified_tag(elem: &smali::android::binary_xml::ManifestElement) -> String {
    match elem.namespace_prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}:{}", elem.tag),
        _ => elem.tag.clone(),
    }
}

fn push_attribute(
    attr: &smali::android::binary_xml::ManifestAttribute,
    chunks: &mut Vec<glass_arch_arm64::Chunk>,
) {
    use glass_arch_arm64::{Chunk, ChunkKind};
    use smali::android::binary_xml::ManifestValue;

    let mk =
        |text: String, kind: ChunkKind| Chunk { text, kind, target: None, target_text: None };

    let name = match attr.namespace_prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}:{}", attr.name),
        _ => attr.name.clone(),
    };
    chunks.push(mk(name, ChunkKind::Modifier));
    chunks.push(mk("=".to_string(), ChunkKind::Punct));
    match &attr.value {
        ManifestValue::String(s) => {
            chunks.push(mk(format!("\"{s}\""), ChunkKind::String));
        }
        ManifestValue::Boolean(b) => {
            chunks.push(mk(format!("\"{b}\""), ChunkKind::Immediate));
        }
        ManifestValue::Integer(n) => {
            chunks.push(mk(format!("\"{n}\""), ChunkKind::Immediate));
        }
        ManifestValue::Hex(h) => {
            chunks.push(mk(format!("\"0x{h:x}\""), ChunkKind::Immediate));
        }
        ManifestValue::Reference(r) => {
            chunks.push(mk(format!("\"@0x{r:x}\""), ChunkKind::Type));
        }
    }
}

pub fn hex_row_for_addr(rows: &[HexRow], addr: u64) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, r) in rows.iter().enumerate() {
        if let HexRow::Bytes { address, bytes } = r {
            let end = address + bytes.len() as u64;
            if *address <= addr && addr < end {
                return Some(i);
            }
            if *address <= addr {
                best = Some(i);
            } else {
                break;
            }
        }
    }
    best
}

/// Find the row index whose instruction address is `addr`, or the
/// nearest one below it. Used to scroll to a clicked address.
/// Build the global search index from a loaded bundle. Called on a
/// background thread because string-scanning a multi-MB rodata is the
/// slow part (still well under a second on typical APKs).
///
/// Indexed:
///   - Native symbols across artifacts (display = demangled).
///   - DEX classes, methods, fields.
///   - Printable ASCII strings ≥4 chars from non-text non-bss
///     non-debug, non-zero-base sections.
///   - Section names (so "rodata" jumps to that section).
pub fn build_search_index(bundle: &LoadedBundle) -> SearchIndex {
    let mut entries: Vec<SearchEntry> = Vec::new();
    let mut artifact_label: std::collections::HashMap<glass_db::ArtifactId, String> =
        std::collections::HashMap::new();

    // Walk text_sections to get a per-artifact "first text section we
    // know about" so symbol jumps have an enclosing section. Also
    // collect a friendly label per artifact (from labels[] when we can
    // find one).
    for ((aid, _name), _) in bundle.text_sections.iter() {
        artifact_label
            .entry(aid.clone())
            .or_insert_with(|| short_artifact_label(bundle, aid));
    }

    // Native symbols.
    for (aid, sm) in bundle.symbol_maps.iter() {
        let alabel = artifact_label
            .entry(aid.clone())
            .or_insert_with(|| short_artifact_label(bundle, aid))
            .clone();
        for sym in sm.iter() {
            // Determine the section containing the symbol — prefer text
            // for code symbols. Fall back to "?" for synthetic FDE-only.
            let section = bundle
                .text_section_for_addr(aid, sym.address)
                .or_else(|| bundle.data_section_for_addr(aid, sym.address))
                .map(|s| s.to_string())
                .unwrap_or_default();
            let jump = if !section.is_empty()
                && bundle.text_sections.contains_key(&(aid.clone(), section.clone()))
            {
                SearchJump::Listing {
                    artifact: aid.clone(),
                    section: section.clone(),
                    addr: sym.address,
                }
            } else if !section.is_empty() {
                SearchJump::Hex {
                    artifact: aid.clone(),
                    section: section.clone(),
                    addr: sym.address,
                }
            } else {
                SearchJump::SectionMap { artifact: aid.clone() }
            };
            entries.push(SearchEntry {
                display: sym.display_name.clone(),
                chip: if section.is_empty() {
                    alabel.clone()
                } else {
                    format!("{section} · {alabel}")
                },
                kind_glyph: "ƒ",
                jump,
            });
        }
    }

    // DEX classes / methods / fields. The class name we surface is the
    // dot-separated Java form ("com.example.Foo"); methods read as
    // "Foo.bar()" with the simple class name to keep the row short.
    let kinds_iter = bundle.kinds.iter().enumerate();
    for (leaf_id, k) in kinds_iter {
        if let LeafKind::SmaliClass { class_jni } = k {
            let display = jni_to_dotted(class_jni);
            let simple = display.rsplit('.').next().unwrap_or(&display).to_string();
            entries.push(SearchEntry {
                display: display.clone(),
                chip: "class".to_string(),
                kind_glyph: "Ⓒ",
                jump: SearchJump::SmaliClass {
                    class_jni: class_jni.clone(),
                },
            });
            // Look up the smali class body to enumerate methods + fields.
            // We use the leaf_id to grab the pre-rendered smali (we kept
            // the bodies for SmaliClass leaves). Cheaper alternative: keep
            // a sidecar list of methods/fields per class at load time.
            // For v1 we parse from the rendered smali body — quick: scan
            // for ".method " and ".field " line prefixes.
            if let Some(body) = bundle.bodies.get(leaf_id) {
                for line in body.lines() {
                    let trimmed = line.trim_start();
                    if let Some(rest) = trimmed.strip_prefix(".method ") {
                        // ".method public static foo()V" → take last token.
                        if let Some(name) = rest.split_whitespace().last() {
                            entries.push(SearchEntry {
                                display: format!("{simple}.{name}"),
                                chip: format!("method · {display}"),
                                kind_glyph: "ƒ",
                                jump: SearchJump::SmaliClass {
                                    class_jni: class_jni.clone(),
                                },
                            });
                        }
                    } else if let Some(rest) = trimmed.strip_prefix(".field ") {
                        if let Some(name) = rest.split_whitespace().last() {
                            entries.push(SearchEntry {
                                display: format!("{simple}.{name}"),
                                chip: format!("field · {display}"),
                                kind_glyph: "ᕀ",
                                jump: SearchJump::SmaliClass {
                                    class_jni: class_jni.clone(),
                                },
                            });
                        }
                    }
                }
            }
        }
    }

    // Section names — useful for "rodata" style searches.
    for (aid, sections) in bundle.native_sections.iter() {
        let alabel = artifact_label
            .entry(aid.clone())
            .or_insert_with(|| short_artifact_label(bundle, aid))
            .clone();
        for sec in sections.iter() {
            entries.push(SearchEntry {
                display: sec.name.to_string(),
                chip: format!("section · {alabel}"),
                kind_glyph: "▤",
                jump: SearchJump::SectionMap { artifact: aid.clone() },
            });
        }
    }

    // Strings from data sections. Skip text sections (covered by listing
    // comments), bss (no bytes), debug (noisy / wrong addressing).
    let mut string_count: usize = 0;
    const MAX_STRINGS: usize = 20_000;
    for ((aid, name), ds) in bundle.data_sections.iter() {
        if ds.kind == NativeSectionKind::Bss
            || ds.kind == NativeSectionKind::Debug
            || ds.base == 0
        {
            continue;
        }
        let alabel = artifact_label
            .entry(aid.clone())
            .or_insert_with(|| short_artifact_label(bundle, aid))
            .clone();
        let bytes: &[u8] = ds.bytes.as_ref();
        let mut i = 0;
        while i < bytes.len() {
            if !is_printable(bytes[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && is_printable(bytes[i]) {
                i += 1;
            }
            let end = i;
            // require NUL terminator to keep noise down
            let nul_terminated = i < bytes.len() && bytes[i] == 0;
            if !nul_terminated {
                continue;
            }
            let len = end - start;
            if len < 4 {
                continue;
            }
            let s = match std::str::from_utf8(&bytes[start..end]) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            };
            let addr = ds.base + start as u64;
            entries.push(SearchEntry {
                display: s,
                chip: format!("string · {} · {}", name, alabel),
                kind_glyph: "\"",
                jump: SearchJump::Hex {
                    artifact: aid.clone(),
                    section: name.clone(),
                    addr,
                },
            });
            string_count += 1;
            if string_count >= MAX_STRINGS {
                break;
            }
        }
        if string_count >= MAX_STRINGS {
            break;
        }
    }

    SearchIndex { entries }
}

fn is_printable(b: u8) -> bool {
    (0x20..=0x7e).contains(&b) || b == b'\t'
}

fn short_artifact_label(bundle: &LoadedBundle, aid: &glass_db::ArtifactId) -> String {
    // Best-effort: scan the tree for a leaf with a "<libname>" label
    // attached to this artifact, else fall back to short hash.
    for (i, k) in bundle.kinds.iter().enumerate() {
        let matches = match k {
            LeafKind::Listing { artifact, .. } => artifact == aid,
            LeafKind::Hex { artifact, .. } => artifact == aid,
            LeafKind::SectionMap { artifact } => artifact == aid,
            _ => false,
        };
        if matches {
            if let Some(label) = bundle.labels.get(i) {
                // SectionMap labels read "<libname> (overview)"; strip that.
                let s = label.as_ref();
                if let Some(prefix) = s.strip_suffix(" (overview)") {
                    return prefix.to_string();
                }
                return s.to_string();
            }
        }
    }
    aid.to_string()
}

fn jni_to_dotted(jni: &str) -> String {
    let trimmed = jni.strip_prefix('L').unwrap_or(jni);
    let trimmed = trimmed.strip_suffix(';').unwrap_or(trimmed);
    trimmed.replace('/', ".")
}

pub fn listing_row_for_addr(rows: &[ListingRow], addr: u64) -> Option<usize> {
    // Linear is fine for now — listings are at most ~200k rows. A
    // BTreeMap<address, row_index> would scale better; revisit when
    // we have a binary that struggles.
    let mut best: Option<usize> = None;
    for (i, r) in rows.iter().enumerate() {
        if let ListingRow::Instruction { address, .. } = r {
            if *address <= addr {
                best = Some(i);
            } else {
                break;
            }
        }
    }
    best
}

/// Lightweight, GPU-friendly section descriptor used by the SectionMap
/// view and (later) the SymbolTable / HexDump views.
#[derive(Clone, Debug)]
pub struct SectionInfo {
    pub name: SharedString,
    pub address: u64,
    pub size: u64,
    pub kind: NativeSectionKind,
    /// Convenience: this section's percentage of the artifact's total
    /// section span. Precomputed so the renderer is O(N).
    pub fraction: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSectionKind {
    Text,
    Data,
    Rodata,
    Bss,
    Debug,
    Other,
}

impl NativeSectionKind {
    fn from_armv8(k: armv8_encode::container::SectionKind) -> Self {
        use armv8_encode::container::SectionKind as K;
        match k {
            K::Text => Self::Text,
            K::Data => Self::Data,
            K::Rodata => Self::Rodata,
            K::Bss => Self::Bss,
            K::Debug => Self::Debug,
            K::Other => Self::Other,
        }
    }

    /// IDA-ish palette. Picked so adjacent sections in the strip remain
    /// distinguishable at small widths on a dark background.
    fn colour(self) -> u32 {
        match self {
            Self::Text => 0x4f7cff,   // blue
            Self::Data => 0x4cb964,   // green
            Self::Rodata => 0x4cc8b9, // teal
            Self::Bss => 0x6b6b75,    // grey
            Self::Debug => 0xa57ad6,  // violet
            Self::Other => 0x8a8a92,  // pale grey
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Text => "code",
            Self::Data => "data",
            Self::Rodata => "rodata",
            Self::Bss => "bss",
            Self::Debug => "debug",
            Self::Other => "other",
        }
    }
}

/// Lighten an opaque 0xRRGGBB by ~25% per channel. Used to give the
/// hovered section in the bar a "this is the one" lift.
// ---- Listing row palette + renderer ----------------------------------------

const LISTING_ROW_HEIGHT: f32 = 22.;
const LISTING_GUTTER_WIDTH: f32 = 32.;
// 16 hex chars + a couple of px of slack. Dropped the `0x` prefix —
// the column is exclusively addresses, so the marker is redundant and
// the saved width keeps the address from wrapping inside Courier.
const LISTING_ADDR_WIDTH: f32 = 170.;
// 4 bytes shown as "XX XX XX XX" (11 chars) plus generous padding.
const LISTING_BYTES_WIDTH: f32 = 140.;
const LISTING_MNEMONIC_WIDTH: f32 = 80.;
/// Min row width so long operand+comment lines have somewhere to slide
/// under a horizontal scroll. ARM64 instructions max out under 200
/// chars at Courier's ~10px/glyph — 2400 px is well past any practical
/// content while keeping the scroll responsive.
const LISTING_ROW_MIN_WIDTH: f32 = 2400.;

// ---- smali tokenizer -------------------------------------------------------

/// Tokenise a single line of smali into coloured chunks. Tiny
/// hand-rolled lexer — smali's grammar is small enough that a
/// per-character pass is the right level. Falls back to a single
/// `Plain` chunk for anything we don't recognise so unknown syntax
/// still renders.
fn tokenize_smali_line(line: &str) -> Vec<glass_arch_arm64::Chunk> {
    use glass_arch_arm64::{Chunk, ChunkKind};
    let mut out: Vec<Chunk> = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;

    let push = |out: &mut Vec<Chunk>, text: String, kind: ChunkKind| {
        if !text.is_empty() {
            out.push(Chunk { text, kind, target: None, target_text: None });
        }
    };

    while i < bytes.len() {
        let c = bytes[i] as char;
        // Whitespace — emit as-is, plain. Preserves leading indent.
        if c == ' ' || c == '\t' {
            let start = i;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Plain);
            continue;
        }
        // Line comments — everything from `#` to end of line.
        if c == '#' {
            push(&mut out, line[i..].to_string(), ChunkKind::Comment);
            break;
        }
        // String literals "..." (smali allows simple escapes).
        if c == '"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::String);
            continue;
        }
        // Directives: a literal `.` followed by an identifier. Always
        // top-of-line in real smali, but we don't constrain on column.
        if c == '.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < bytes.len() && smali_ident_byte(bytes[i]) {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Directive);
            continue;
        }
        // Labels: `:foo`, `:cond_0`. Often appear inline (goto targets).
        if c == ':' && i + 1 < bytes.len() && smali_ident_byte(bytes[i + 1]) {
            let start = i;
            i += 1;
            while i < bytes.len() && smali_ident_byte(bytes[i]) {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Label);
            continue;
        }
        // Type signatures starting with `L` (class refs) or `[`
        // (arrays). End at `;` for classes, or after a single
        // primitive char for `[I`/`[Z`/etc.
        if (c == 'L' || c == '[')
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'L' || bytes[i + 1] == b'[')
            && !preceded_by_ident_char(bytes, i)
        {
            let start = i;
            let mut j = i;
            // Skip nested array brackets.
            while j < bytes.len() && bytes[j] == b'[' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'L' {
                // L...; class ref.
                j += 1;
                while j < bytes.len() && bytes[j] != b';' {
                    j += 1;
                }
                if j < bytes.len() {
                    j += 1; // include `;`
                }
            } else if j < bytes.len() && b"VZBSCIJFD".contains(&bytes[j]) {
                // Primitive after possible `[`s.
                j += 1;
            } else {
                // Not actually a type — treat the `L` as plain.
                push(&mut out, (c as char).to_string(), ChunkKind::Plain);
                i += 1;
                continue;
            }
            let type_text = line[start..j].to_string();
            push(&mut out, type_text.clone(), ChunkKind::Type);
            i = j;
            // Method/field reference: `<Type>;->name(sig)ret` or
            // `<Type>;->FIELD:Type`. If we see `->` immediately after
            // the Type and the next chars look like a method name,
            // consume `->` as Punct and the whole `name(sig)ret` as
            // a single MethodName chunk so the renderer can wire a
            // deep-link click. Field refs (`->FIELD:Type`) we leave
            // alone — they're rarer and the trailing `:Type` already
            // colours.
            if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'>' {
                // Peek the identifier + signature.
                let arrow_start = i;
                let mut k = i + 2;
                let name_start = k;
                while k < bytes.len() && smali_ident_byte(bytes[k]) {
                    k += 1;
                }
                let name_end = k;
                // Method form: an `(` must follow the name.
                if name_end > name_start && k < bytes.len() && bytes[k] == b'(' {
                    // Consume the args until the matching `)` then
                    // the return type (single primitive or class).
                    while k < bytes.len() && bytes[k] != b')' {
                        k += 1;
                    }
                    if k < bytes.len() && bytes[k] == b')' {
                        k += 1; // consume `)`
                    }
                    // Return type: primitive (single char) or class
                    // `L...;` or array prefix `[`s. Consume one full
                    // type token.
                    while k < bytes.len() && bytes[k] == b'[' {
                        k += 1;
                    }
                    if k < bytes.len() {
                        if bytes[k] == b'L' {
                            k += 1;
                            while k < bytes.len() && bytes[k] != b';' {
                                k += 1;
                            }
                            if k < bytes.len() {
                                k += 1;
                            }
                        } else if b"VZBSCIJFD".contains(&bytes[k]) {
                            k += 1;
                        }
                    }
                    // Emit Punct("->"), MethodName(name+sig+ret) with
                    // target_text="ClassRef" + arrow + body.
                    push(&mut out, "->".to_string(), ChunkKind::Punct);
                    let method_body = line[name_start..k].to_string();
                    let full_ref = format!("{type_text}->{method_body}");
                    out.push(glass_arch_arm64::Chunk {
                        text: method_body,
                        kind: ChunkKind::MethodName,
                        target: None,
                        target_text: Some(full_ref),
                    });
                    i = k;
                    continue;
                }
                // Not a method ref — leave the `->` to be tokenised
                // normally on the next iteration.
                let _ = arrow_start;
            }
            continue;
        }
        // Standalone primitive type after `)` — return type of a
        // method signature, e.g. the trailing V in `(II)V`.
        if matches!(c, 'V' | 'Z' | 'B' | 'S' | 'C' | 'I' | 'J' | 'F' | 'D')
            && preceded_by_byte(bytes, i, b')')
        {
            push(&mut out, c.to_string(), ChunkKind::Type);
            i += 1;
            continue;
        }
        // Registers: vN or pN where N is a decimal number.
        if (c == 'v' || c == 'p')
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && !preceded_by_ident_char(bytes, i)
        {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Register);
            continue;
        }
        // Numeric literals: optional sign, optional `0x`, then digits.
        // Trailing suffixes `L`/`l`/`f`/`F`/`s`/`S`/`t`/`T` for
        // long/float/short/byte hints — eat them as part of the number.
        if c.is_ascii_digit()
            || (c == '-'
                && i + 1 < bytes.len()
                && bytes[i + 1].is_ascii_digit())
        {
            let start = i;
            if c == '-' {
                i += 1;
            }
            if i + 1 < bytes.len() && bytes[i] == b'0' && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
                i += 2;
                while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
            } else {
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
            }
            // Optional numeric type-tag suffix.
            if i < bytes.len() && b"LlFfDdSstT".contains(&bytes[i]) {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Immediate);
            continue;
        }
        // Identifiers: opcodes, modifiers, method/field names, etc.
        // We need a classification pass after the identifier is read.
        if smali_ident_start(c as u8) {
            let start = i;
            while i < bytes.len() && smali_ident_byte(bytes[i]) {
                i += 1;
            }
            let word = &line[start..i];
            let kind = classify_smali_word(word, out.last().map(|c| c.text.as_str()));
            push(&mut out, word.to_string(), kind);
            continue;
        }
        // Everything else (commas, braces, arrows, `=`, `->`, `;`, …)
        // is punctuation. Group consecutive punct chars.
        let start = i;
        while i < bytes.len() && is_smali_punct(bytes[i]) {
            i += 1;
        }
        if i == start {
            // Defensive: if nothing matched, advance by one byte so we
            // can never hang the renderer on an unrecognised char. This
            // also handles non-ASCII bytes (multi-byte UTF-8 sequences
            // get bumped a byte at a time and rendered as Plain). We
            // step by UTF-8 char boundary to avoid producing invalid
            // string slices.
            let step = utf8_char_byte_len(bytes[i]);
            let end = (i + step).min(bytes.len());
            push(&mut out, line[i..end].to_string(), ChunkKind::Plain);
            i = end;
        } else {
            push(&mut out, line[start..i].to_string(), ChunkKind::Punct);
        }
    }

    out
}

fn smali_ident_start(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'<' | b'$')
}

fn smali_ident_byte(b: u8) -> bool {
    smali_ident_start(b) || b.is_ascii_digit() || matches!(b, b'-' | b'/' | b'>')
}

fn is_smali_punct(b: u8) -> bool {
    matches!(
        b,
        b',' | b'{' | b'}' | b'(' | b')' | b';' | b'=' | b'!' | b'?'
        | b'-' | b'>' | b'/' | b'+' | b'*' | b'&' | b'|' | b'^' | b'~'
        | b'.' | b':' | b'@'
    )
}

fn preceded_by_ident_char(bytes: &[u8], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let prev = bytes[i - 1];
    prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'$'
}

fn preceded_by_byte(bytes: &[u8], i: usize, b: u8) -> bool {
    i > 0 && bytes[i - 1] == b
}

/// Byte length of the UTF-8 char starting at `b`. Falls back to 1 for
/// any invalid lead byte so a malformed input still advances the
/// tokenizer instead of looping.
/// If `chunk_text` is a class JNI (`Lcom/example/Foo;`, possibly
/// preceded by array markers `[[Lcom/...;`) return the bare class JNI
/// without the leading `[`s. Returns `None` for primitives, method
/// signatures, and any non-class type.
fn extract_class_jni(chunk_text: &str) -> Option<&str> {
    let trimmed = chunk_text.trim_start_matches('[');
    if trimmed.starts_with('L') && trimmed.ends_with(';') && trimmed.len() > 2 {
        Some(trimmed)
    } else {
        None
    }
}

fn utf8_char_byte_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xc0 {
        1 // continuation byte without lead — treat as standalone
    } else if b < 0xe0 {
        2
    } else if b < 0xf0 {
        3
    } else {
        4
    }
}

/// Classify a bare identifier from a smali line. `prev_text` lets us
/// pick `Modifier` for words that appear in a declaration after a
/// `.method`/`.field`/`.class` directive — but the line-based parser
/// gets only this hint, not the full file-level state.
fn classify_smali_word(word: &str, prev_text: Option<&str>) -> glass_arch_arm64::ChunkKind {
    use glass_arch_arm64::ChunkKind as K;
    match word {
        // Common smali opcodes — rendered as Mnemonic. Not exhaustive:
        // anything we miss falls through to Plain which is fine — at
        // least the rest of the line keeps its colouring.
        "invoke-virtual" | "invoke-direct" | "invoke-static" | "invoke-super"
        | "invoke-interface" | "invoke-virtual/range" | "invoke-direct/range"
        | "invoke-static/range" | "invoke-super/range" | "invoke-interface/range"
        | "invoke-polymorphic" | "invoke-polymorphic/range"
        | "invoke-custom" | "invoke-custom/range"
        | "move" | "move/from16" | "move/16" | "move-wide" | "move-wide/from16"
        | "move-wide/16" | "move-object" | "move-object/from16"
        | "move-object/16" | "move-result" | "move-result-wide"
        | "move-result-object" | "move-exception"
        | "return" | "return-void" | "return-wide" | "return-object"
        | "const" | "const/4" | "const/16" | "const/high16" | "const-wide"
        | "const-wide/16" | "const-wide/32" | "const-wide/high16"
        | "const-string" | "const-string/jumbo" | "const-class"
        | "monitor-enter" | "monitor-exit" | "check-cast" | "instance-of"
        | "array-length" | "new-instance" | "new-array" | "filled-new-array"
        | "filled-new-array/range" | "fill-array-data" | "throw"
        | "goto" | "goto/16" | "goto/32"
        | "packed-switch" | "sparse-switch"
        | "cmpl-float" | "cmpg-float" | "cmpl-double" | "cmpg-double" | "cmp-long"
        | "if-eq" | "if-ne" | "if-lt" | "if-ge" | "if-gt" | "if-le"
        | "if-eqz" | "if-nez" | "if-ltz" | "if-gez" | "if-gtz" | "if-lez"
        | "nop" => K::Mnemonic,

        // Java access / declaration modifiers. Smali lists them
        // inline after `.method`/`.field`/`.class`.
        "public" | "private" | "protected" | "static" | "final" | "abstract"
        | "native" | "synchronized" | "transient" | "volatile" | "synthetic"
        | "bridge" | "varargs" | "constructor" | "interface" | "enum"
        | "annotation" | "declared-synchronized" | "strict" | "strictfp"
        | "fpstrict" => K::Modifier,

        _ => {
            // Opcode family detection by prefix — covers all the
            // `iget-*`, `iput-*`, `aget-*`, `aput-*`, `sget-*`,
            // `sput-*`, `add-*`, `sub-*`, `mul-*`, `div-*`,
            // `rem-*`, `and-*`, `or-*`, `xor-*`, `shl-*`,
            // `shr-*`, `ushr-*` variants we don't list literally.
            let mnemonic_prefixes: &[&str] = &[
                "iget", "iput", "sget", "sput", "aget", "aput",
                "add-", "sub-", "mul-", "div-", "rem-",
                "and-", "or-", "xor-", "shl-", "shr-", "ushr-",
                "neg-", "not-",
                "int-to-", "long-to-", "float-to-", "double-to-",
            ];
            if mnemonic_prefixes.iter().any(|p| word.starts_with(p)) {
                return K::Mnemonic;
            }
            // Anything else — could be a class name, method name, field
            // name. Without more context default to Plain.
            let _ = prev_text;
            K::Plain
        }
    }
}

// ---- listing palette -------------------------------------------------------

const COLOUR_ADDR: u32 = 0x8a8a92;
const COLOUR_BYTES: u32 = 0x676770;
const COLOUR_MNEMONIC: u32 = 0x6fc3df;
const COLOUR_REGISTER: u32 = 0xa8c5ff;
const COLOUR_IMMEDIATE: u32 = 0xf4a55a;
const COLOUR_ADDRESS_OP: u32 = 0xf3d27a;
const COLOUR_SHIFT: u32 = 0xb6b6c0;
const COLOUR_CONDITION: u32 = 0xc191ff;
const COLOUR_PUNCT: u32 = 0x808088;
const COLOUR_COMMENT: u32 = 0x6e9c5d;
const COLOUR_SYMBOL_HEADER: u32 = 0xfff39c;
const COLOUR_BB_SEPARATOR: u32 = 0x3a3a42;
const COLOUR_PLAIN: u32 = 0xd6d6d6;
// Smali-specific palette (sits next to the ARM64 colours above; reuse
// of common kinds — Register, Immediate, Punct, Plain — keeps the two
// views visually consistent).
const COLOUR_DIRECTIVE: u32 = 0xff9c6e; // top-level .pragma — warm orange
const COLOUR_MODIFIER: u32 = 0xc191ff;  // public/static/final — violet
const COLOUR_LABEL: u32 = 0xff8fc1;     // :cond_0 etc. — rose
const COLOUR_TYPE: u32 = 0xf3d27a;      // resolvable Lcom/example/Foo; — warm yellow
const COLOUR_TYPE_EXTERNAL: u32 = 0x8c7a4a; // external class refs (java/* etc.) — muted
const COLOUR_STRING: u32 = 0xa5d678;    // "..." string literals — green
/// Subtle accent tint for the selected row. Brighter than the panel
/// background but dim enough not to fight the colour-coded chunks.
const COLOUR_ROW_SELECTED: u32 = 0x2e3245;

fn chunk_colour(kind: glass_arch_arm64::ChunkKind) -> u32 {
    use glass_arch_arm64::ChunkKind as K;
    match kind {
        K::Mnemonic => COLOUR_MNEMONIC,
        K::Register => COLOUR_REGISTER,
        K::Immediate => COLOUR_IMMEDIATE,
        K::Address => COLOUR_ADDRESS_OP,
        K::Shift => COLOUR_SHIFT,
        K::Condition => COLOUR_CONDITION,
        K::Punct => COLOUR_PUNCT,
        K::Plain => COLOUR_PLAIN,
        K::Directive => COLOUR_DIRECTIVE,
        K::Modifier => COLOUR_MODIFIER,
        K::Label => COLOUR_LABEL,
        K::Comment => COLOUR_COMMENT,
        K::Type => COLOUR_TYPE,
        K::String => COLOUR_STRING,
        // MethodName: colourwise this is a plain identifier. The
        // renderer wraps it in a clickable affordance separately
        // when the method ref resolves.
        K::MethodName => COLOUR_PLAIN,
    }
}

/// Wrap a row's inner content in a horizontal-offset translator. The
/// outer div clips and stretches to the visible viewport; the inner
/// content is the full-width row content shifted by `-h_offset`. When
/// a `ctx` is supplied, the wrapper also handles click-to-select and
/// applies the selection background tint.
fn h_shift(
    inner: gpui::Div,
    h_offset: Pixels,
    row_height: f32,
    row_index: usize,
    ctx: Option<&RowCtx>,
) -> gpui::Stateful<gpui::Div> {
    let is_selected = ctx.map(|c| c.selected_row == Some(row_index)).unwrap_or(false);
    let mut outer = div()
        .id(("listing-row", row_index))
        .h(px(row_height))
        .w_full()
        .overflow_hidden()
        .relative();
    if is_selected {
        outer = outer.bg(rgb(COLOUR_ROW_SELECTED));
    }
    if let Some(ctx) = ctx {
        let weak = ctx.shell.clone();
        outer = outer.on_mouse_down(
            gpui::MouseButton::Left,
            move |_ev, _w, cx: &mut App| {
                if let Some(entity) = weak.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.select_active_row(row_index, cx);
                    });
                }
            },
        );
    }
    outer.child(
        inner
            .absolute()
            .top_0()
            .left(-h_offset)
            .h(px(row_height))
            .w(px(LISTING_ROW_MIN_WIDTH)),
    )
}

/// Context passed into a single row's render — needed so Address
/// chunks can wire click-to-goto handlers and rows can mark themselves
/// as selected.
#[derive(Clone)]
struct RowCtx {
    bundle: LoadedBundle,
    artifact: glass_db::ArtifactId,
    shell: gpui::WeakEntity<Shell>,
    selected_row: Option<usize>,
}

// ---- Hex row palette + renderer --------------------------------------------

const HEX_ROW_HEIGHT: f32 = 22.;
/// Each hex cell is `XX ` (3 chars) — fixed width so column 7 is always
/// at the same x for cursor-style cell highlighting.
const HEX_CELL_WIDTH: f32 = 26.;
/// 16 cells × HEX_CELL_WIDTH plus a small gap before ASCII.
const HEX_BYTES_WIDTH: f32 = 16.0 * HEX_CELL_WIDTH + 8.;
/// 16 ASCII chars × ~10 px Courier.
const HEX_ASCII_WIDTH: f32 = 160.;
/// Width of a hex row's content. Wider than listing because the bytes
/// column is fixed; ASCII can extend past min to keep horizontal scroll
/// margin similar to listing.
const HEX_ROW_MIN_WIDTH: f32 = 2400.;

/// Brighter cell highlight that sits on top of the row tint to mark
/// the specific byte under the cursor.
const COLOUR_BYTE_SELECTED: u32 = 0x4f7cff;

fn render_hex_row(
    row: &HexRow,
    row_index: usize,
    h_offset: Pixels,
    ctx: Option<&RowCtx>,
    selected_byte_addr: Option<u64>,
) -> gpui::Stateful<gpui::Div> {
    let mut row_div = match row {
        HexRow::SymbolHeader { name } => h_shift(
            div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                .child(div().w(px(LISTING_GUTTER_WIDTH)).h_full().flex_shrink_0())
                .child(
                    div()
                        .text_color(rgb(COLOUR_SYMBOL_HEADER))
                        .child(format!("{name}:")),
                ),
            h_offset,
            HEX_ROW_HEIGHT,
            row_index,
            ctx,
        ),
        HexRow::Bytes { address, bytes } => {
            // Pre-build the cells. We render up to 16 hex cells and 16
            // ASCII glyphs, padding shorter rows with empty cells so
            // column alignment is preserved.
            let mut hex_cells = div()
                .w(px(HEX_BYTES_WIDTH))
                .flex_shrink_0()
                .flex()
                .flex_row()
                .pr_2();
            let mut ascii_cells = div()
                .w(px(HEX_ASCII_WIDTH))
                .flex_shrink_0()
                .flex()
                .flex_row();
            for i in 0..16 {
                let byte = bytes.get(i).copied();
                let cell_addr = address + i as u64;
                let is_selected_byte = selected_byte_addr == Some(cell_addr);
                let hex_text = match byte {
                    Some(b) => format!("{b:02x}"),
                    None => "  ".to_string(),
                };
                let ascii_glyph = match byte {
                    Some(b) if (0x20..=0x7e).contains(&b) => (b as char).to_string(),
                    Some(_) => ".".to_string(),
                    None => " ".to_string(),
                };
                // Attach a click handler to each cell so the user can
                // pick out a specific byte. The handler sets both
                // `selected_row` (via select_active_row) and the byte
                // address. Rows without a context can't be selected at
                // all — that path is only used when ctx is `None`,
                // which we never reach in the runtime render.
                let make_cell = |w: Pixels, text: String| {
                    let mut c = div()
                        .id(("hex-cell", row_index * 16 + i))
                        .w(w)
                        .whitespace_nowrap()
                        .text_color(rgb(COLOUR_BYTES))
                        .child(text);
                    if is_selected_byte {
                        c = c.bg(rgb(COLOUR_BYTE_SELECTED)).text_color(rgb(0xffffff));
                    }
                    if let Some(ctx) = ctx {
                        if byte.is_some() {
                            let weak = ctx.shell.clone();
                            c = c.cursor_pointer().on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_ev, _w, cx: &mut App| {
                                    if let Some(entity) = weak.upgrade() {
                                        cx.update_entity(&entity, |shell, cx| {
                                            shell.select_active_row(row_index, cx);
                                            shell.select_byte(cell_addr, cx);
                                        });
                                    }
                                    cx.stop_propagation();
                                },
                            );
                        }
                    }
                    c
                };
                hex_cells = hex_cells.child(make_cell(px(HEX_CELL_WIDTH), hex_text));
                ascii_cells = ascii_cells.child(make_cell(px(10.), ascii_glyph));
            }

            let inner = div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                .child(div().w(px(LISTING_GUTTER_WIDTH)).h_full().flex_shrink_0())
                .child(
                    div()
                        .w(px(LISTING_ADDR_WIDTH))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .pr_4()
                        .text_color(rgb(COLOUR_ADDR))
                        .child(format!("{address:016x}")),
                )
                .child(hex_cells)
                .child(ascii_cells);
            h_shift(inner, h_offset, HEX_ROW_HEIGHT, row_index, ctx)
        }
    };

    // Click on hex/ASCII cells should also set `selected_byte_addr`.
    // h_shift already attached a row-level on_mouse_down for selection;
    // we layer on cell-level handlers in the body. But row_div is now
    // `Stateful<Div>`, and child-level handlers can be attached at row
    // build time. For now the row-level handler is enough — the byte
    // selection comes from the operand-click path inside listings; we
    // can add cell click in a polish pass.
    let _ = &mut row_div;
    row_div
}

fn render_listing_row_with(
    row: &ListingRow,
    row_index: usize,
    h_offset: Pixels,
    ctx: Option<&RowCtx>,
) -> gpui::Stateful<gpui::Div> {
    match row {
        ListingRow::SymbolHeader { name } => h_shift(
            div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                .child(
                    // gutter — keep alignment with instruction rows
                    div().w(px(LISTING_GUTTER_WIDTH)).h_full().flex_shrink_0(),
                )
                .child(
                    div()
                        .text_color(rgb(COLOUR_SYMBOL_HEADER))
                        .child(format!("{name}:")),
                ),
            h_offset,
            LISTING_ROW_HEIGHT,
            row_index,
            ctx,
        ),
        ListingRow::BasicBlockSeparator => h_shift(
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(
                    div().w(px(LISTING_GUTTER_WIDTH)).h_full().flex_shrink_0(),
                )
                .child(
                    div()
                        .flex_1()
                        .h(px(1.))
                        .bg(rgb(COLOUR_BB_SEPARATOR)),
                ),
            h_offset,
            8.,
            row_index,
            ctx,
        ),
        ListingRow::Instruction {
            address,
            bytes,
            mnemonic,
            operands,
            comment,
        } => {
            let mut row_div = div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                // CF arrow gutter — reserved space, empty for now.
                .child(div().w(px(LISTING_GUTTER_WIDTH)).h_full().flex_shrink_0())
                // Address column. No `0x` prefix — the column is
                // unambiguous and the saved width keeps Courier from
                // wrapping the 16-hex string.
                .child(
                    div()
                        .w(px(LISTING_ADDR_WIDTH))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .pr_4()
                        .text_color(rgb(COLOUR_ADDR))
                        .child(format!("{address:016x}")),
                )
                // Bytes column with right-padding so the next column
                // doesn't kiss it.
                .child(
                    div()
                        .w(px(LISTING_BYTES_WIDTH))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .pr_4()
                        .text_color(rgb(COLOUR_BYTES))
                        .child(format!(
                            "{:02x} {:02x} {:02x} {:02x}",
                            bytes[0], bytes[1], bytes[2], bytes[3]
                        )),
                )
                // Mnemonic column.
                .child(
                    div()
                        .w(px(LISTING_MNEMONIC_WIDTH))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .text_color(rgb(COLOUR_MNEMONIC))
                        .child(mnemonic.clone()),
                );
            // Operands as separately-coloured chunks. Address chunks
            // (with a known target) are wrapped in a clickable element
            // so the user can jump to the destination — yellow text gets
            // an underline on hover and a "goto …" tooltip.
            // Operands sit at their natural width — *not* flex_1 —
            // because we want the trailing comment to render to the
            // right of them. `flex_1` here would consume the full
            // remaining row width and squeeze the comment to zero.
            let mut ops_row = div().flex().flex_row().flex_shrink_0();
            for (i, chunk) in operands.iter().enumerate() {
                let base = div()
                    .id(("addr-chunk", i))
                    .text_color(rgb(chunk_colour(chunk.kind)))
                    .child(SharedString::from(chunk.text.clone()));
                let cell: gpui::AnyElement = match (chunk.kind, chunk.target, ctx) {
                    (glass_arch_arm64::ChunkKind::Address, Some(t), Some(ctx)) => {
                        let weak = ctx.shell.clone();
                        let artifact = ctx.artifact.clone();
                        // Prefer text (Listing reuse). Fall back to data
                        // (Hex). If neither matches, the operand is
                        // styled clickable but the handler is omitted.
                        let target = ctx
                            .bundle
                            .text_section_for_addr(&ctx.artifact, t)
                            .map(|s| (s.to_string(), false))
                            .or_else(|| {
                                ctx.bundle
                                    .data_section_for_addr(&ctx.artifact, t)
                                    .map(|s| (s.to_string(), true))
                            });
                        let display = chunk.text.clone();
                        let tooltip_label = format!("goto {display}");
                        let mut el = base
                            .cursor_pointer()
                            .hover(|this| this.underline());
                        if let Some((section_name, is_data)) = target {
                            el = el.on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_ev, _w, cx: &mut App| {
                                    let Some(entity) = weak.upgrade() else { return };
                                    let artifact = artifact.clone();
                                    let section_name = section_name.clone();
                                    cx.update_entity(&entity, |shell, cx| {
                                        if is_data {
                                            shell.open_hex_in_new_tab(
                                                artifact,
                                                section_name,
                                                t,
                                                cx,
                                            );
                                        } else {
                                            shell.open_listing_at(
                                                artifact,
                                                section_name,
                                                t,
                                                cx,
                                            );
                                        }
                                    });
                                },
                            );
                        }
                        el.tooltip(move |_window, cx| {
                            cx.new(|_| TextTooltip {
                                text: SharedString::from(tooltip_label.clone()),
                            })
                            .into()
                        })
                        .into_any_element()
                    }
                    _ => base.into_any_element(),
                };
                ops_row = ops_row.child(cell);
            }
            row_div = row_div.child(ops_row);
            // Trailing comment, if any.
            if !comment.is_empty() {
                row_div = row_div.child(
                    div()
                        .ml_4()
                        .text_color(rgb(COLOUR_COMMENT))
                        .child(comment.clone()),
                );
            }
            h_shift(row_div, h_offset, LISTING_ROW_HEIGHT, row_index, ctx)
        }
    }
}

/// Minimal text-only tooltip view. gpui's `tooltip()` API wants an
/// `AnyView`, so we build a tiny entity that just renders its string.
pub struct TextTooltip {
    text: SharedString,
}

impl Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(rgb(0x18181c))
            .border_1()
            .border_color(rgb(0x36363c))
            .rounded_sm()
            .text_xs()
            .text_color(rgb(0xf2f2f2))
            .font_family("Menlo")
            .child(self.text.clone())
    }
}

fn brighten(rgb_hex: u32) -> u32 {
    let r = ((rgb_hex >> 16) & 0xff) as u32;
    let g = ((rgb_hex >> 8) & 0xff) as u32;
    let b = (rgb_hex & 0xff) as u32;
    let lift = |c: u32| (c + (0xff - c) / 4).min(0xff);
    (lift(r) << 16) | (lift(g) << 8) | lift(b)
}

/// What clicking a leaf in the tree should open.
#[derive(Debug, Clone)]
pub enum LeafKind {
    /// Lifted smali for a DEX class. The string is the JNI signature —
    /// stable across DEX reshuffles, so it's also the persistence key.
    SmaliClass { class_jni: String },
    /// AArch64 linear listing over a native artifact's `__text`.
    Listing {
        artifact: glass_db::ArtifactId,
        section: String,
    },
    /// Tabulated hex view of a non-text section.
    Hex {
        artifact: glass_db::ArtifactId,
        section: String,
    },
    /// Section map (overview) for a native artifact.
    SectionMap { artifact: glass_db::ArtifactId },
    /// AndroidManifest.xml viewer.
    Manifest,
}

impl LoadedBundle {
    /// Find the leaf that backs a given persisted tab state. Returns
    /// `None` if the bundle no longer contains it (e.g. a class
    /// disappeared between sessions).
    pub fn resolve(&self, state: &glass_db::TabState) -> Option<LeafId> {
        use glass_db::TabState as TS;
        match state {
            TS::SmaliClass { class_jni } => self.kinds.iter().enumerate().find_map(|(i, k)| {
                match k {
                    LeafKind::SmaliClass { class_jni: this } if this == class_jni => {
                        Some(LeafId(i))
                    }
                    _ => None,
                }
            }),
            TS::Listing { artifact, section, .. } => {
                self.kinds.iter().enumerate().find_map(|(i, k)| match k {
                    LeafKind::Listing { artifact: a, section: s } if a == artifact && s == section => {
                        Some(LeafId(i))
                    }
                    _ => None,
                })
            }
            TS::Hex { artifact, section, .. } => {
                self.kinds.iter().enumerate().find_map(|(i, k)| match k {
                    LeafKind::Hex { artifact: a, section: s } if a == artifact && s == section => {
                        Some(LeafId(i))
                    }
                    _ => None,
                })
            }
            TS::SectionMap { artifact } => {
                self.kinds.iter().enumerate().find_map(|(i, k)| match k {
                    LeafKind::SectionMap { artifact: a } if a == artifact => Some(LeafId(i)),
                    _ => None,
                })
            }
            TS::Manifest => self.kinds.iter().enumerate().find_map(|(i, k)| match k {
                LeafKind::Manifest => Some(LeafId(i)),
                _ => None,
            }),
            _ => None,
        }
    }

    /// Find which section of a native artifact contains `addr`. Only
    /// returns sections we can disassemble (`Text` kind today).
    pub fn text_section_for_addr(
        &self,
        artifact: &glass_db::ArtifactId,
        addr: u64,
    ) -> Option<&str> {
        let sections = self.native_sections.get(artifact)?;
        for sec in sections {
            if sec.kind == NativeSectionKind::Text
                && addr >= sec.address
                && addr < sec.address.saturating_add(sec.size)
            {
                return Some(sec.name.as_ref());
            }
        }
        None
    }

    /// Mirror of `text_section_for_addr` for non-text sections that we
    /// could open in the hex view. BSS is excluded (no on-disk bytes).
    pub fn data_section_for_addr(
        &self,
        artifact: &glass_db::ArtifactId,
        addr: u64,
    ) -> Option<&str> {
        let sections = self.native_sections.get(artifact)?;
        for sec in sections {
            if sec.kind != NativeSectionKind::Text
                && sec.kind != NativeSectionKind::Bss
                && addr >= sec.address
                && addr < sec.address.saturating_add(sec.size)
            {
                return Some(sec.name.as_ref());
            }
        }
        None
    }
}

/// Tree of groups + leaves. Groups can nest arbitrarily (package hierarchy);
/// leaves are the clickable items that have a body.
#[derive(Debug)]
pub struct Tree {
    pub roots: Vec<Node>,
}

#[derive(Debug)]
pub enum Node {
    Group {
        label: SharedString,
        children: Vec<Node>,
    },
    Leaf {
        label: SharedString,
        leaf_id: LeafId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafId(pub usize);

fn snapshot_apk_with_progress(
    apk: ApkBundle,
    progress: Arc<Mutex<Progress>>,
    display_label: String,
) -> Result<LoadedBundle> {
    // Hash each artifact as we touch its bytes.
    let mut artifact_ids: Vec<glass_db::ArtifactId> = Vec::new();
    let mut native_sections: std::collections::HashMap<
        glass_db::ArtifactId,
        Vec<SectionInfo>,
    > = std::collections::HashMap::new();
    let mut symbol_maps: std::collections::HashMap<
        glass_db::ArtifactId,
        glass_arch_arm64::SymbolMap,
    > = std::collections::HashMap::new();
    let mut text_sections: std::collections::HashMap<
        (glass_db::ArtifactId, String),
        TextSectionBytes,
    > = std::collections::HashMap::new();
    let mut data_sections: std::collections::HashMap<
        (glass_db::ArtifactId, String),
        DataSectionBytes,
    > = std::collections::HashMap::new();
    for dex in &apk.dex_files {
        artifact_ids.push(glass_db::ArtifactId::from_bytes(&dex.bytes));
    }
    // Hash each native lib once; reuse the resulting id in the tree
    // loop further down. Hashing a 23 MB lib twice costs real time on
    // big APKs (coc-jigsaw has libg.so at 23 MB).
    let lib_artifact_ids: Vec<glass_db::ArtifactId> = apk
        .native_libs
        .iter()
        .map(|lib| glass_db::ArtifactId::from_bytes(&lib.binary.bytes))
        .collect();
    for (lib, aid) in apk.native_libs.iter().zip(lib_artifact_ids.iter()) {
        let aid = aid.clone();
        native_sections.insert(aid.clone(), build_section_info(&lib.binary.container));
        symbol_maps.insert(
            aid.clone(),
            glass_arch_arm64::SymbolMap::build(&lib.binary.container),
        );
        // armv8-encode parses the container regardless of architecture
        // but its decoder is AArch64-only. For non-AArch64 (x86_64,
        // armeabi-v7a, etc.) the listing would render meaningless
        // AArch64 reads of the bytes, so we register every section —
        // including text — as data so the UI routes clicks to the hex
        // view instead.
        let arch = lib.binary.container.architecture;
        let aarch64 =
            matches!(arch, armv8_encode::container::Architecture::Aarch64);
        for sec in &lib.binary.container.sections {
            let kind = NativeSectionKind::from_armv8(sec.kind);
            let is_text =
                matches!(sec.kind, armv8_encode::container::SectionKind::Text);
            if aarch64 && is_text {
                text_sections.insert(
                    (aid.clone(), sec.name.clone()),
                    TextSectionBytes {
                        base: sec.address,
                        bytes: Arc::new(sec.bytes.clone()),
                    },
                );
            } else if !sec.bytes.is_empty() {
                data_sections.insert(
                    (aid.clone(), sec.name.clone()),
                    DataSectionBytes {
                        base: sec.address,
                        bytes: Arc::new(sec.bytes.clone()),
                        kind,
                    },
                );
            }
        }
        artifact_ids.push(aid);
    }

    let mut bodies: Vec<SharedString> = Vec::new();
    let mut origins: Vec<SharedString> = Vec::new();
    let mut labels: Vec<SharedString> = Vec::new();
    let mut kinds: Vec<LeafKind> = Vec::new();
    let mut roots: Vec<Node> = Vec::new();

    // Manifest leaf at the very top — first thing a reverser usually
    // looks at. Only emit when we actually parsed a manifest.
    let manifest_rows: Vec<ManifestRow> = match apk.manifest.as_ref() {
        Some(m) => {
            let leaf_id = LeafId(bodies.len());
            bodies.push(SharedString::from(""));
            origins.push(SharedString::from("manifest"));
            labels.push(SharedString::from("AndroidManifest.xml"));
            kinds.push(LeafKind::Manifest);
            roots.push(Node::Leaf {
                label: SharedString::from("AndroidManifest.xml"),
                leaf_id,
            });
            flatten_manifest(m)
        }
        None => Vec::new(),
    };

    // Count total classes up-front for a determinate bar.
    let mut total_classes = 0usize;
    for dex in &apk.dex_files {
        total_classes += dex.classes()?.len();
    }
    if let Ok(mut p) = progress.lock() {
        p.phase = SharedString::from("Lifting smali…");
        p.current = 0;
        p.total = total_classes;
    }

    let mut processed = 0usize;
    for dex in &apk.dex_files {
        let classes = dex.classes()?;
        let dex_origin = SharedString::from(dex.name.clone());
        let mut pkg_root = PkgBuilder::default();
        for class in classes {
            let id = LeafId(bodies.len());
            bodies.push(SharedString::from(class.to_smali()));
            origins.push(dex_origin.clone());
            let jni = class.name.to_string();
            let parts = split_jni_class_name(&jni);
            labels.push(SharedString::from(
                parts.last().cloned().unwrap_or_else(|| jni.clone()),
            ));
            kinds.push(LeafKind::SmaliClass { class_jni: jni.clone() });
            pkg_root.insert(&parts, id);
            processed += 1;
            // Updating shared state every class would thrash the lock. The
            // UI polls at ~30fps so a coarser cadence here is plenty.
            if processed % 64 == 0 {
                if let Ok(mut p) = progress.lock() {
                    p.current = processed;
                }
            }
        }
        roots.push(Node::Group {
            label: dex_origin,
            children: pkg_root.finish(),
        });
    }
    if let Ok(mut p) = progress.lock() {
        p.current = processed;
    }

    if !apk.native_libs.is_empty() {
        if let Ok(mut p) = progress.lock() {
            p.phase = SharedString::from("Disassembling native…");
            p.current = 0;
            p.total = apk.native_libs.len();
        }
        use std::collections::BTreeMap;
        let mut by_abi: BTreeMap<String, Vec<Node>> = BTreeMap::new();
        for (i, lib) in apk.native_libs.iter().enumerate() {
            let lib_aid = lib_artifact_ids[i].clone();
            let arch = lib.binary.container.architecture;
            let aarch64 =
                matches!(arch, armv8_encode::container::Architecture::Aarch64);

            // Overview leaf (SectionMap), then one leaf per actual text
            // section. ELF uses `.text`, Mach-O uses `__text`, and some
            // ELF variants split text across `.text.startup` etc. —
            // we surface them all as siblings under the lib.
            //
            // For non-AArch64 libs (armeabi-v7a, x86_64, …) we can't
            // disassemble — armv8-encode is AArch64-only. Emit Hex
            // leaves so clicking takes the user to the raw byte view
            // instead of a fake disassembly.
            let overview_id = LeafId(bodies.len());
            bodies.push(SharedString::from("")); // SectionMap renders its own body
            origins.push(SharedString::from(format!("lib/{}", lib.abi)));
            labels.push(SharedString::from(format!("{} (overview)", lib.name)));
            kinds.push(LeafKind::SectionMap { artifact: lib_aid.clone() });

            let mut children: Vec<Node> = vec![Node::Leaf {
                label: SharedString::from("Overview"),
                leaf_id: overview_id,
            }];
            for sec in &lib.binary.container.sections {
                if !matches!(sec.kind, armv8_encode::container::SectionKind::Text) {
                    continue;
                }
                let leaf_id = LeafId(bodies.len());
                bodies.push(SharedString::from(""));
                origins.push(SharedString::from(format!("lib/{}", lib.abi)));
                labels.push(SharedString::from(sec.name.clone()));
                if aarch64 {
                    kinds.push(LeafKind::Listing {
                        artifact: lib_aid.clone(),
                        section: sec.name.clone(),
                    });
                } else {
                    kinds.push(LeafKind::Hex {
                        artifact: lib_aid.clone(),
                        section: sec.name.clone(),
                    });
                }
                children.push(Node::Leaf {
                    label: SharedString::from(sec.name.clone()),
                    leaf_id,
                });
            }

            // Tag the lib group label with arch when we can't
            // disassemble — makes "why is this in hex?" self-evident.
            let lib_label = if aarch64 {
                lib.name.clone()
            } else {
                format!("{} ({})", lib.name, arch_label(arch))
            };
            by_abi
                .entry(lib.abi.clone())
                .or_default()
                .push(Node::Group {
                    label: SharedString::from(lib_label),
                    children,
                });
            if let Ok(mut p) = progress.lock() {
                p.current = i + 1;
            }
        }
        let mut lib_children = Vec::new();
        for (abi, libs) in by_abi {
            lib_children.push(Node::Group {
                label: SharedString::from(abi),
                children: libs,
            });
        }
        roots.push(Node::Group {
            label: SharedString::from("lib"),
            children: lib_children,
        });
    }

    let method_lines = build_method_line_index(&kinds, &bodies);

    // Derive BundleId from the artifact IDs themselves. Same content-
    // addressed guarantee (any DEX/lib changes ⇒ new bundle id) at a
    // tiny fraction of the cost of hashing the whole APK file.
    let mut bundle_hasher = blake3::Hasher::new();
    for aid in &artifact_ids {
        bundle_hasher.update(aid.as_bytes());
    }
    let bundle_id = glass_db::BundleId::from_raw(*bundle_hasher.finalize().as_bytes());

    Ok(LoadedBundle {
        title: format!("Glass — {}", apk.path.display()),
        tree: Arc::new(Tree { roots }),
        bodies: Arc::new(bodies),
        origins: Arc::new(origins),
        labels: Arc::new(labels),
        kinds: Arc::new(kinds),
        bundle_id: Some(bundle_id),
        artifact_ids: Arc::new(artifact_ids),
        display_label,
        native_sections: Arc::new(native_sections),
        symbol_maps: Arc::new(symbol_maps),
        text_sections: Arc::new(text_sections),
        data_sections: Arc::new(data_sections),
        method_lines: Arc::new(method_lines),
        manifest_rows: Arc::new(manifest_rows),
    })
}

/// Render an `Info.plist` into the same depth-indented `ManifestRow`
/// stream that the XML viewer consumes. We render in plist-XML style
/// (`<key>`/`<string>` etc.) so it reads naturally to anyone used to
/// staring at Info.plist files.
pub fn flatten_info_plist(info: &glass_mobile::InfoPlist) -> Vec<ManifestRow> {
    use glass_arch_arm64::{Chunk, ChunkKind};
    let mk = |text: String, kind: ChunkKind| Chunk {
        text,
        kind,
        target: None,
        target_text: None,
    };
    let mut rows = Vec::new();
    rows.push(ManifestRow {
        depth: 0,
        chunks: Arc::new(vec![
            mk("<".into(), ChunkKind::Punct),
            mk("plist".into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]),
    });
    rows.push(ManifestRow {
        depth: 1,
        chunks: Arc::new(vec![
            mk("<".into(), ChunkKind::Punct),
            mk("dict".into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]),
    });
    if let Some(v) = info.extras.as_ref() {
        flatten_plist_value(v, 2, &mut rows);
    }
    rows.push(ManifestRow {
        depth: 1,
        chunks: Arc::new(vec![
            mk("</".into(), ChunkKind::Punct),
            mk("dict".into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]),
    });
    rows.push(ManifestRow {
        depth: 0,
        chunks: Arc::new(vec![
            mk("</".into(), ChunkKind::Punct),
            mk("plist".into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]),
    });
    rows
}

fn flatten_plist_value(
    value: &plist::Value,
    depth: usize,
    rows: &mut Vec<ManifestRow>,
) {
    use glass_arch_arm64::{Chunk, ChunkKind};
    let mk = |text: String, kind: ChunkKind| Chunk {
        text,
        kind,
        target: None,
        target_text: None,
    };
    let scalar = |tag: &str, raw: String, kind: ChunkKind| {
        vec![
            mk("<".into(), ChunkKind::Punct),
            mk(tag.into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
            mk(raw, kind),
            mk("</".into(), ChunkKind::Punct),
            mk(tag.into(), ChunkKind::Directive),
            mk(">".into(), ChunkKind::Punct),
        ]
    };

    match value {
        plist::Value::Dictionary(dict) => {
            for (key, child) in dict.iter() {
                rows.push(ManifestRow {
                    depth,
                    chunks: Arc::new(vec![
                        mk("<".into(), ChunkKind::Punct),
                        mk("key".into(), ChunkKind::Directive),
                        mk(">".into(), ChunkKind::Punct),
                        mk(key.to_string(), ChunkKind::String),
                        mk("</".into(), ChunkKind::Punct),
                        mk("key".into(), ChunkKind::Directive),
                        mk(">".into(), ChunkKind::Punct),
                    ]),
                });
                flatten_plist_value(child, depth, rows);
            }
        }
        plist::Value::Array(arr) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(vec![
                    mk("<".into(), ChunkKind::Punct),
                    mk("array".into(), ChunkKind::Directive),
                    mk(">".into(), ChunkKind::Punct),
                ]),
            });
            for item in arr {
                flatten_plist_value(item, depth + 1, rows);
            }
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(vec![
                    mk("</".into(), ChunkKind::Punct),
                    mk("array".into(), ChunkKind::Directive),
                    mk(">".into(), ChunkKind::Punct),
                ]),
            });
        }
        plist::Value::String(s) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("string", s.clone(), ChunkKind::String)),
            });
        }
        plist::Value::Integer(n) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("integer", n.to_string(), ChunkKind::Modifier)),
            });
        }
        plist::Value::Real(r) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("real", r.to_string(), ChunkKind::Modifier)),
            });
        }
        plist::Value::Boolean(b) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(vec![
                    mk("<".into(), ChunkKind::Punct),
                    mk(if *b { "true" } else { "false" }.into(), ChunkKind::Directive),
                    mk("/>".into(), ChunkKind::Punct),
                ]),
            });
        }
        plist::Value::Date(d) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("date", format!("{d:?}"), ChunkKind::String)),
            });
        }
        plist::Value::Data(bytes) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar(
                    "data",
                    format!("[{} bytes]", bytes.len()),
                    ChunkKind::Comment,
                )),
            });
        }
        plist::Value::Uid(uid) => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(scalar("uid", format!("{uid:?}"), ChunkKind::Modifier)),
            });
        }
        _ => {
            rows.push(ManifestRow {
                depth,
                chunks: Arc::new(vec![mk("<unknown/>".into(), ChunkKind::Comment)]),
            });
        }
    }
}

/// IPA snapshot. Mirrors `snapshot_apk_with_progress` but for iOS:
/// Info.plist + main executable + frameworks/dylibs.
fn snapshot_ipa_with_progress(
    ipa: IpaBundle,
    progress: Arc<Mutex<Progress>>,
    display_label: String,
) -> Result<LoadedBundle> {
    let mut artifact_ids: Vec<glass_db::ArtifactId> = Vec::new();
    let mut native_sections: std::collections::HashMap<
        glass_db::ArtifactId,
        Vec<SectionInfo>,
    > = std::collections::HashMap::new();
    let mut symbol_maps: std::collections::HashMap<
        glass_db::ArtifactId,
        glass_arch_arm64::SymbolMap,
    > = std::collections::HashMap::new();
    let mut text_sections: std::collections::HashMap<
        (glass_db::ArtifactId, String),
        TextSectionBytes,
    > = std::collections::HashMap::new();
    let mut data_sections: std::collections::HashMap<
        (glass_db::ArtifactId, String),
        DataSectionBytes,
    > = std::collections::HashMap::new();

    let mut bodies: Vec<SharedString> = Vec::new();
    let mut origins: Vec<SharedString> = Vec::new();
    let mut labels: Vec<SharedString> = Vec::new();
    let mut kinds: Vec<LeafKind> = Vec::new();
    let mut roots: Vec<Node> = Vec::new();

    // Info.plist leaf at the top — first thing a reverser checks for
    // the bundle id, executable name, and entitlements clues.
    let info_rows = flatten_info_plist(&ipa.info);
    {
        let leaf_id = LeafId(bodies.len());
        bodies.push(SharedString::from(""));
        origins.push(SharedString::from("plist"));
        labels.push(SharedString::from("Info.plist"));
        kinds.push(LeafKind::Manifest);
        roots.push(Node::Leaf {
            label: SharedString::from("Info.plist"),
            leaf_id,
        });
    }

    // Helper to register one native artifact (main exec or framework
    // binary). Returns its ArtifactId and a Group node summarising it.
    let mut register_artifact = |bytes: &[u8],
                                 container: &armv8_encode::container::Container,
                                 display_name: String,
                                 origin: String|
     -> (glass_db::ArtifactId, Node) {
        let aid = glass_db::ArtifactId::from_bytes(bytes);
        native_sections.insert(aid.clone(), build_section_info(container));
        symbol_maps.insert(aid.clone(), glass_arch_arm64::SymbolMap::build(container));

        let arch = container.architecture;
        let aarch64 =
            matches!(arch, armv8_encode::container::Architecture::Aarch64);
        for sec in &container.sections {
            let kind = NativeSectionKind::from_armv8(sec.kind);
            let is_text =
                matches!(sec.kind, armv8_encode::container::SectionKind::Text);
            if aarch64 && is_text {
                text_sections.insert(
                    (aid.clone(), sec.name.clone()),
                    TextSectionBytes {
                        base: sec.address,
                        bytes: Arc::new(sec.bytes.clone()),
                    },
                );
            } else if !sec.bytes.is_empty() {
                data_sections.insert(
                    (aid.clone(), sec.name.clone()),
                    DataSectionBytes {
                        base: sec.address,
                        bytes: Arc::new(sec.bytes.clone()),
                        kind,
                    },
                );
            }
        }

        let overview_id = LeafId(bodies.len());
        bodies.push(SharedString::from(""));
        origins.push(SharedString::from(origin.clone()));
        labels.push(SharedString::from(format!("{display_name} (overview)")));
        kinds.push(LeafKind::SectionMap { artifact: aid.clone() });

        let mut children: Vec<Node> = vec![Node::Leaf {
            label: SharedString::from("Overview"),
            leaf_id: overview_id,
        }];
        for sec in &container.sections {
            if !matches!(sec.kind, armv8_encode::container::SectionKind::Text) {
                continue;
            }
            let leaf_id = LeafId(bodies.len());
            bodies.push(SharedString::from(""));
            origins.push(SharedString::from(origin.clone()));
            labels.push(SharedString::from(sec.name.clone()));
            if aarch64 {
                kinds.push(LeafKind::Listing {
                    artifact: aid.clone(),
                    section: sec.name.clone(),
                });
            } else {
                kinds.push(LeafKind::Hex {
                    artifact: aid.clone(),
                    section: sec.name.clone(),
                });
            }
            children.push(Node::Leaf {
                label: SharedString::from(sec.name.clone()),
                leaf_id,
            });
        }
        let group_label = if aarch64 {
            display_name
        } else {
            format!("{display_name} ({})", arch_label(arch))
        };
        let node = Node::Group {
            label: SharedString::from(group_label),
            children,
        };
        (aid, node)
    };

    if let Ok(mut p) = progress.lock() {
        p.phase = SharedString::from("Disassembling native…");
        p.current = 0;
        p.total = 1 + ipa.frameworks.len();
    }
    let mut progressed = 0usize;
    let bump = |progress: &Arc<Mutex<Progress>>, progressed: &mut usize| {
        *progressed += 1;
        if let Ok(mut p) = progress.lock() {
            p.current = *progressed;
        }
    };

    // Main executable.
    if let Some(bin) = &ipa.main_executable {
        let display_name = ipa
            .info
            .executable
            .clone()
            .unwrap_or_else(|| "main".to_string());
        let (aid, node) = register_artifact(
            &bin.bytes,
            &bin.container,
            display_name,
            "main".to_string(),
        );
        artifact_ids.push(aid);
        roots.push(node);
    }
    bump(&progress, &mut progressed);

    // Frameworks / dylibs. Each ships its own arm64-sliced bytes; parse
    // and register them just like the main exec.
    if !ipa.frameworks.is_empty() {
        let mut fw_children: Vec<Node> = Vec::new();
        for fw in &ipa.frameworks {
            let bin = match Arm64Binary::from_bytes(
                PathBuf::from(&fw.archive_path),
                fw.bytes.clone(),
            ) {
                Ok(b) => b,
                Err(e) => {
                    tracing::debug!("skipping {}: {e}", fw.name);
                    bump(&progress, &mut progressed);
                    continue;
                }
            };
            let (aid, node) = register_artifact(
                &bin.bytes,
                &bin.container,
                fw.name.clone(),
                "Frameworks".to_string(),
            );
            artifact_ids.push(aid);
            fw_children.push(node);
            bump(&progress, &mut progressed);
        }
        if !fw_children.is_empty() {
            roots.push(Node::Group {
                label: SharedString::from("Frameworks"),
                children: fw_children,
            });
        }
    }

    let method_lines = build_method_line_index(&kinds, &bodies);

    let mut bundle_hasher = blake3::Hasher::new();
    for aid in &artifact_ids {
        bundle_hasher.update(aid.as_bytes());
    }
    let bundle_id = glass_db::BundleId::from_raw(*bundle_hasher.finalize().as_bytes());

    Ok(LoadedBundle {
        title: format!("Glass — {}", ipa.path.display()),
        tree: Arc::new(Tree { roots }),
        bodies: Arc::new(bodies),
        origins: Arc::new(origins),
        labels: Arc::new(labels),
        kinds: Arc::new(kinds),
        bundle_id: Some(bundle_id),
        artifact_ids: Arc::new(artifact_ids),
        display_label,
        native_sections: Arc::new(native_sections),
        symbol_maps: Arc::new(symbol_maps),
        text_sections: Arc::new(text_sections),
        data_sections: Arc::new(data_sections),
        method_lines: Arc::new(method_lines),
        manifest_rows: Arc::new(info_rows),
    })
}

/// Walk every SmaliClass leaf in the bundle, scan its body, record the
/// line index of each `.method` declaration, and key it by the same
/// `Class;->name(sig)ret` form a smali method-ref takes. Single linear
/// pass per class, on the load thread.
///
/// Smali method-decl lines look like:
///   `.method public static foo(Lcom/Foo;I)V`
///
/// We pluck the trailing token (which is `name(sig)ret`) and pair it
/// with the class JNI to form the key.
fn build_method_line_index(
    kinds: &[LeafKind],
    bodies: &[SharedString],
) -> std::collections::HashMap<String, (LeafId, usize)> {
    let mut map = std::collections::HashMap::new();
    for (i, k) in kinds.iter().enumerate() {
        let LeafKind::SmaliClass { class_jni } = k else { continue };
        let Some(body) = bodies.get(i) else { continue };
        for (line_no, raw) in body.lines().enumerate() {
            let trimmed = raw.trim_start();
            let Some(after) = trimmed.strip_prefix(".method ") else { continue };
            // Last whitespace-separated token = name(sig)ret.
            let Some(method_decl) = after.split_whitespace().last() else { continue };
            let key = format!("{class_jni}->{method_decl}");
            map.entry(key).or_insert((LeafId(i), line_no));
        }
    }
    map
}

/// Short tag for non-AArch64 architectures, shown in the tree label.
fn arch_label(arch: armv8_encode::container::Architecture) -> &'static str {
    use armv8_encode::container::Architecture as A;
    match arch {
        A::Aarch64 => "arm64",
        A::Arm => "arm32",
        A::Other => "other",
    }
}

/// Snapshot section metadata for a native artifact into a UI-friendly form.
fn build_section_info(container: &armv8_encode::container::Container) -> Vec<SectionInfo> {
    // Total on-disk + bss size across all sections — we draw the bar
    // proportional to size rather than file offset because Mach-O segments
    // and ELF sections have very different on-disk vs. virtual extents.
    // Using `size` keeps the strip readable on a typical ARM64 .so where
    // .text dominates.
    let total: u64 = container.sections.iter().map(|s| s.size).sum();
    container
        .sections
        .iter()
        .map(|s| SectionInfo {
            name: SharedString::from(s.name.clone()),
            address: s.address,
            size: s.size,
            kind: NativeSectionKind::from_armv8(s.kind),
            fraction: if total == 0 {
                0.
            } else {
                s.size as f32 / total as f32
            },
        })
        .collect()
}

pub fn snapshot_arm64(bin: Arm64Binary) -> Result<LoadedBundle> {
    let body = format_arm64(&bin);
    let display_label = bin
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("binary")
        .to_string();
    let aid = glass_db::ArtifactId::from_bytes(&bin.bytes);
    let mut native_sections = std::collections::HashMap::new();
    native_sections.insert(aid.clone(), build_section_info(&bin.container));
    let mut symbol_maps = std::collections::HashMap::new();
    symbol_maps.insert(aid.clone(), glass_arch_arm64::SymbolMap::build(&bin.container));
    let mut text_sections = std::collections::HashMap::new();
    let mut data_sections = std::collections::HashMap::new();
    for sec in &bin.container.sections {
        let kind = NativeSectionKind::from_armv8(sec.kind);
        if matches!(sec.kind, armv8_encode::container::SectionKind::Text) {
            text_sections.insert(
                (aid.clone(), sec.name.clone()),
                TextSectionBytes {
                    base: sec.address,
                    bytes: Arc::new(sec.bytes.clone()),
                },
            );
        } else if !sec.bytes.is_empty() {
            data_sections.insert(
                (aid.clone(), sec.name.clone()),
                DataSectionBytes {
                    base: sec.address,
                    bytes: Arc::new(sec.bytes.clone()),
                    kind,
                },
            );
        }
    }
    // Build leaves: Overview + one Listing per actual text section.
    let mut tree_roots: Vec<Node> = Vec::new();
    let mut bodies: Vec<SharedString> = Vec::new();
    let mut origins: Vec<SharedString> = Vec::new();
    let mut labels_v: Vec<SharedString> = Vec::new();
    let mut kinds_v: Vec<LeafKind> = Vec::new();

    bodies.push(SharedString::from(""));
    origins.push(SharedString::from("arm64"));
    labels_v.push(SharedString::from(format!("{display_label} (overview)")));
    kinds_v.push(LeafKind::SectionMap { artifact: aid.clone() });
    tree_roots.push(Node::Leaf {
        label: SharedString::from("Overview"),
        leaf_id: LeafId(0),
    });
    let _ = body; // legacy: built earlier; no longer used now that Listing reads from text_sections
    for sec in &bin.container.sections {
        if !matches!(sec.kind, armv8_encode::container::SectionKind::Text) {
            continue;
        }
        let leaf_id = LeafId(bodies.len());
        bodies.push(SharedString::from(""));
        origins.push(SharedString::from("arm64"));
        labels_v.push(SharedString::from(sec.name.clone()));
        kinds_v.push(LeafKind::Listing {
            artifact: aid.clone(),
            section: sec.name.clone(),
        });
        tree_roots.push(Node::Leaf {
            label: SharedString::from(sec.name.clone()),
            leaf_id,
        });
    }

    Ok(LoadedBundle {
        title: format!("Glass — {}", bin.path.display()),
        tree: Arc::new(Tree { roots: tree_roots }),
        bodies: Arc::new(bodies),
        origins: Arc::new(origins),
        labels: Arc::new(labels_v),
        kinds: Arc::new(kinds_v),
        bundle_id: None,
        artifact_ids: Arc::new(vec![aid]),
        display_label,
        native_sections: Arc::new(native_sections),
        symbol_maps: Arc::new(symbol_maps),
        text_sections: Arc::new(text_sections),
        data_sections: Arc::new(data_sections),
        method_lines: Arc::new(std::collections::HashMap::new()),
        manifest_rows: Arc::new(Vec::new()),
    })
}

fn format_arm64(bin: &Arm64Binary) -> String {
    let rows = match glass_arch_arm64::linear_sweep(&bin.container) {
        Ok(r) => r,
        Err(e) => return format!("(disassembly failed: {e})"),
    };
    let mut out = String::new();
    for row in rows.iter().take(5000) {
        out.push_str(&format!("0x{:016x}  {}\n", row.address, row.text));
    }
    if rows.len() > 5000 {
        out.push_str(&format!("... ({} more rows truncated)\n", rows.len() - 5000));
    }
    out
}

/// Split `Lcom/example/Foo$Bar;` -> `["com", "example", "Foo$Bar"]`.
fn split_jni_class_name(jni: &str) -> Vec<String> {
    let trimmed = jni
        .strip_prefix('L')
        .unwrap_or(jni)
        .strip_suffix(';')
        .unwrap_or(jni);
    trimmed.split('/').map(|s| s.to_string()).collect()
}

#[derive(Default)]
struct PkgBuilder {
    /// child name -> subtree (or leaf flagged via `leaf`).
    subpkgs: std::collections::BTreeMap<String, PkgBuilder>,
    leaf: Option<LeafId>,
    /// Direct class leaves at this package level (insertion preserved by
    /// pushing into a vec).
    classes: Vec<(String, LeafId)>,
}

impl PkgBuilder {
    fn insert(&mut self, parts: &[String], id: LeafId) {
        match parts {
            [] => self.leaf = Some(id),
            [name] => self.classes.push((name.clone(), id)),
            [head, tail @ ..] => self.subpkgs.entry(head.clone()).or_default().insert(tail, id),
        }
    }

    fn finish(self) -> Vec<Node> {
        let mut out = Vec::new();
        // Packages first (sorted by BTreeMap), then classes (insertion order).
        for (name, sub) in self.subpkgs {
            let children = sub.finish();
            if children.is_empty() {
                continue;
            }
            // Collapse single-child package chains for compactness:
            //   com -> example -> Foo  shown as  com.example
            //                                       Foo
            let (label, children) = collapse_chain(name, children);
            out.push(Node::Group {
                label: SharedString::from(label),
                children,
            });
        }
        for (name, id) in self.classes {
            out.push(Node::Leaf {
                label: SharedString::from(name),
                leaf_id: id,
            });
        }
        out
    }
}

fn collapse_chain(mut label: String, mut children: Vec<Node>) -> (String, Vec<Node>) {
    while children.len() == 1 {
        if let Node::Group { label: child_label, children: child_kids } = &children[0] {
            label = format!("{label}.{child_label}");
            let next = child_kids.clone_or_take();
            children = next;
        } else {
            break;
        }
    }
    (label, children)
}

// Small helper trait so collapse_chain can move out of a borrowed Vec.
trait CloneOrTake {
    fn clone_or_take(&self) -> Vec<Node>;
}
impl CloneOrTake for Vec<Node> {
    fn clone_or_take(&self) -> Vec<Node> {
        self.iter().map(clone_node).collect()
    }
}
fn clone_node(n: &Node) -> Node {
    match n {
        Node::Group { label, children } => Node::Group {
            label: label.clone(),
            children: children.iter().map(clone_node).collect(),
        },
        Node::Leaf { label, leaf_id } => Node::Leaf {
            label: label.clone(),
            leaf_id: *leaf_id,
        },
    }
}

// ---- visible row flattening -------------------------------------------------

#[derive(Clone)]
enum RowKind {
    Group {
        path: Vec<usize>,
        expanded: bool,
        label: SharedString,
    },
    Leaf {
        leaf_id: LeafId,
        label: SharedString,
    },
}

#[derive(Clone)]
struct VisibleRow {
    depth: usize,
    kind: RowKind,
}

fn flatten(tree: &Tree, expanded: &Expanded) -> Vec<VisibleRow> {
    let mut out = Vec::new();
    for (idx, node) in tree.roots.iter().enumerate() {
        walk(node, &mut vec![idx], 0, expanded, &mut out);
    }
    out
}

fn walk(
    node: &Node,
    path: &mut Vec<usize>,
    depth: usize,
    expanded: &Expanded,
    out: &mut Vec<VisibleRow>,
) {
    match node {
        Node::Group { label, children } => {
            let is_open = expanded.contains(path);
            out.push(VisibleRow {
                depth,
                kind: RowKind::Group {
                    path: path.clone(),
                    expanded: is_open,
                    label: label.clone(),
                },
            });
            if is_open {
                for (i, child) in children.iter().enumerate() {
                    path.push(i);
                    walk(child, path, depth + 1, expanded, out);
                    path.pop();
                }
            }
        }
        Node::Leaf { label, leaf_id } => {
            out.push(VisibleRow {
                depth,
                kind: RowKind::Leaf {
                    leaf_id: *leaf_id,
                    label: label.clone(),
                },
            });
        }
    }
}

#[derive(Default, Clone)]
struct Expanded {
    /// Set of node paths that are expanded.
    open: std::collections::HashSet<Vec<usize>>,
}

impl Expanded {
    fn contains(&self, path: &[usize]) -> bool {
        self.open.contains(path)
    }
    fn toggle(&mut self, path: &[usize]) {
        if !self.open.remove(path) {
            self.open.insert(path.to_vec());
        }
    }
}

// ---- view -------------------------------------------------------------------

/// Runtime tab. Mirrors `glass_db::TabState` but holds the live `ListState`
/// for scrolling — that's why it can't itself be serialized.
///
/// Per-tab scroll memory is automatic: each tab owns its own `ListState`,
/// preserving position across tab switches.
struct Tab {
    /// What this tab represents. Stable across reloads.
    kind: TabKind,
    /// Scroll state for the right pane when this tab is active.
    scroll: ListState,
    /// SmaliClass: cached line split of the body.
    /// Listing: unused (see `listing_rows`).
    lines: Option<Arc<Vec<SharedString>>>,
    /// Listing: precomputed mixed rows (symbol headers, instructions,
    /// basic-block separators). Built lazily on a worker thread.
    listing_rows: Option<Arc<Vec<ListingRow>>>,
    /// While `listing_rows` is being built off-thread, this holds the
    /// shared progress structure so the render path can show a bar.
    /// Cleared the moment rows land.
    listing_progress: Option<Arc<Mutex<Progress>>>,
    /// Horizontal scroll offset for the right-pane body. We can't nest
    /// gpui's `overflow_x_scroll` around a `list()` — the list owns
    /// scroll events — so each row translates its inner content by
    /// `-h_offset` and the outer row clips with `overflow_hidden`.
    /// Driven by trackpad/shift+wheel and by a custom scrollbar.
    h_offset: Pixels,
    /// One-shot scroll target consumed on the next active-tab paint.
    /// Used when a tab is opened with a request like "jump to 0x1234".
    /// Not persisted — request-by-construction only.
    pending_scroll_addr: Option<u64>,
    /// Smali deep-link target — the line index to scroll to once the
    /// tab's smali body is materialised.
    pending_smali_scroll_line: Option<usize>,
    /// Index of the currently-selected row in this tab's row list (or
    /// line list for SmaliClass). The renderer paints a faint accent
    /// background on the matching row. Click-to-select is the basis
    /// for future operations: edit, comment, colour-tag.
    selected_row: Option<usize>,
    /// Hex view: the absolute address of the byte under the user's
    /// cursor, when one is selected. Drives the per-cell highlight
    /// that distinguishes which of a row's 16 bytes is the "target".
    selected_byte_addr: Option<u64>,
    /// Hex view: precomputed rows (lazily built).
    hex_rows: Option<Arc<Vec<HexRow>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TabKind {
    SmaliClass {
        class_jni: String,
    },
    Listing {
        artifact: glass_db::ArtifactId,
        section: String,
    },
    Hex {
        artifact: glass_db::ArtifactId,
        section: String,
    },
    SectionMap {
        artifact: glass_db::ArtifactId,
    },
    Manifest,
}

impl TabKind {
    /// Persistable form — round-trips through `glass-db`.
    fn to_state(&self) -> glass_db::TabState {
        match self {
            TabKind::SmaliClass { class_jni } => glass_db::TabState::SmaliClass {
                class_jni: class_jni.clone(),
            },
            TabKind::Listing { artifact, section } => glass_db::TabState::Listing {
                artifact: artifact.clone(),
                section: section.clone(),
                scroll_top: 0,
            },
            TabKind::Hex { artifact, section } => glass_db::TabState::Hex {
                artifact: artifact.clone(),
                section: section.clone(),
                scroll_top: 0,
            },
            TabKind::SectionMap { artifact } => glass_db::TabState::SectionMap {
                artifact: artifact.clone(),
            },
            TabKind::Manifest => glass_db::TabState::Manifest,
        }
    }

    fn from_kind(kind: &LeafKind) -> Self {
        match kind {
            LeafKind::SmaliClass { class_jni } => TabKind::SmaliClass {
                class_jni: class_jni.clone(),
            },
            LeafKind::Listing { artifact, section } => TabKind::Listing {
                artifact: artifact.clone(),
                section: section.clone(),
            },
            LeafKind::Hex { artifact, section } => TabKind::Hex {
                artifact: artifact.clone(),
                section: section.clone(),
            },
            LeafKind::SectionMap { artifact } => TabKind::SectionMap {
                artifact: artifact.clone(),
            },
            LeafKind::Manifest => TabKind::Manifest,
        }
    }
}

impl Tab {
    fn new(kind: TabKind) -> Self {
        Self {
            kind,
            pending_scroll_addr: None,
            pending_smali_scroll_line: None,
            scroll: ListState::new(0, ListAlignment::Top, px(2000.)),
            lines: None,
            listing_rows: None,
            listing_progress: None,
            h_offset: px(0.),
            selected_row: None,
            selected_byte_addr: None,
            hex_rows: None,
        }
    }
}

struct Shell {
    /// Root focus — the bound key combos (cmd-F etc.) and the
    /// palette's on_key_down only fire when this is focused.
    focus_handle: FocusHandle,
    /// Source path the bundle was loaded from. Used so save_state can
    /// remember where to reopen it from (Open Recent).
    source_path: Option<PathBuf>,
    state: ShellState,
    /// Set while loading. UI reads this on every paint to render the bar.
    progress: Option<Arc<Mutex<Progress>>>,
    expanded: Expanded,
    /// Open tabs in display order.
    tabs: Vec<Tab>,
    active_tab: Option<usize>,
    list_state: ListState,
    visible_count: usize,
    /// Most recently measured pixel width of the tab bar container. Written
    /// by a `canvas` prepaint hook each frame so the next render can decide
    /// how many fixed-width tabs fit.
    tab_bar_width: Pixels,
    /// Whether the overflow dropdown is open.
    overflow_open: bool,
    /// Persistence handle. `None` if the DB couldn't be opened — we still
    /// run, just without restore-on-reopen.
    db: Option<glass_db::Database>,
    /// Bounds of the section-map bar in window coordinates, captured by
    /// the canvas hook. Used to translate mouse positions into a section
    /// index for the hover cursor.
    section_bar_bounds: Bounds<Pixels>,
    /// Index of the section the user is hovering on the bar — drives the
    /// vertical cursor line and the row highlight in the table.
    hovered_section: Option<usize>,
    /// Interpolated address under the bar cursor — used to look up the
    /// covering symbol for the tooltip. `None` when the source of hover
    /// is the table (no horizontal position there) or the cursor has
    /// left the bar.
    bar_cursor_addr: Option<u64>,
    /// Window-coordinate x of the bar cursor, for tooltip positioning.
    bar_cursor_x: Option<Pixels>,
    /// Section-map table scroll state — for auto-revealing the hovered row.
    section_table_scroll: ListState,
    section_table_len: usize,
    /// Search index for the current bundle, built lazily on a background
    /// thread the first time the palette is opened.
    search_index: Option<Arc<SearchIndex>>,
    /// Whether the index is currently being built.
    search_indexing: bool,
    /// Palette modal state. Survives close+reopen — the user's last
    /// query and selection come back when they click the icon again.
    palette_open: bool,
    palette_query: String,
    palette_selected: usize,
    palette_list_state: ListState,
    palette_list_len: usize,
    /// Whether the palette's text input has focus. Set on open and on
    /// any click inside the input area.
    palette_focused: bool,
}

impl Shell {
    fn new(
        path: Option<PathBuf>,
        db: Option<glass_db::Database>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Grab focus immediately so key bindings (cmd-F etc.) work
        // without the user clicking the window first.
        window.focus(&focus_handle, cx);
        let state = if path.is_some() {
            ShellState::Loading
        } else {
            ShellState::Empty
        };
        let source_path = path.clone();
        Self {
            focus_handle,
            source_path,
            state,
            progress: None,
            expanded: Expanded::default(),
            tabs: Vec::new(),
            active_tab: None,
            list_state: ListState::new(0, ListAlignment::Top, px(2000.)),
            visible_count: 0,
            tab_bar_width: px(0.),
            overflow_open: false,
            db,
            section_bar_bounds: Bounds::default(),
            hovered_section: None,
            bar_cursor_addr: None,
            bar_cursor_x: None,
            section_table_scroll: ListState::new(0, ListAlignment::Top, px(2000.)),
            section_table_len: 0,
            search_index: None,
            search_indexing: false,
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            palette_list_state: ListState::new(0, ListAlignment::Top, px(2000.)),
            palette_list_len: 0,
            palette_focused: false,
        }
    }

    fn set_section_bar_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        // Coarse change-detection — avoid notify loops.
        let cur = self.section_bar_bounds;
        let diff = (cur.origin.x - bounds.origin.x).abs()
            + (cur.size.width - bounds.size.width).abs();
        if diff > px(0.5) {
            self.section_bar_bounds = bounds;
            cx.notify();
        }
    }

    fn on_section_bar_move(
        &mut self,
        position: gpui::Point<Pixels>,
        sections: &[SectionInfo],
        cx: &mut Context<Self>,
    ) {
        let bounds = self.section_bar_bounds;
        if bounds.size.width <= px(0.) {
            return;
        }
        let local_x = (position.x - bounds.origin.x).as_f32();
        let width = bounds.size.width.as_f32();
        if local_x < 0. || local_x > width {
            return;
        }
        // Walk sections by accumulated fraction, tracking where each
        // begins so we can interpolate an address within the hit one.
        let mut acc_before = 0.0_f32;
        let target = local_x / width;
        let mut hit: Option<(usize, f32, f32)> = None; // (index, start_frac, frac)
        for (i, sec) in sections.iter().enumerate() {
            let f = sec.fraction.max(0.002);
            let next = acc_before + f;
            if target <= next {
                hit = Some((i, acc_before, f));
                break;
            }
            acc_before = next;
        }
        if hit.is_none() && !sections.is_empty() {
            let last = sections.len() - 1;
            let f = sections[last].fraction.max(0.002);
            hit = Some((last, 1.0 - f, f));
        }
        let (hit_idx, hit_addr) = match hit {
            Some((i, start, f)) => {
                let sec = &sections[i];
                let inner_frac = if f > 0. { (target - start) / f } else { 0. };
                let addr = sec.address + ((sec.size as f32) * inner_frac.clamp(0., 1.)) as u64;
                (Some(i), Some(addr))
            }
            None => (None, None),
        };

        let need_scroll = hit_idx != self.hovered_section;
        if need_scroll
            || self.bar_cursor_addr != hit_addr
            || self.bar_cursor_x != Some(position.x)
        {
            self.hovered_section = hit_idx;
            self.bar_cursor_addr = hit_addr;
            self.bar_cursor_x = Some(position.x);
            if need_scroll {
                if let Some(i) = hit_idx {
                    self.section_table_scroll.scroll_to_reveal_item(i);
                }
            }
            cx.notify();
        }
    }

    /// Clear bar-hover state when the mouse leaves the bar.
    fn on_section_bar_leave(&mut self, cx: &mut Context<Self>) {
        if self.hovered_section.is_some()
            || self.bar_cursor_addr.is_some()
            || self.bar_cursor_x.is_some()
        {
            self.hovered_section = None;
            self.bar_cursor_addr = None;
            self.bar_cursor_x = None;
            cx.notify();
        }
    }

    /// Set the hovered section *without* scrolling the table — used when
    /// the source of the hover is the table itself (rows firing
    /// `on_mouse_move`), so we don't yank the row out from under the
    /// cursor.
    fn set_hovered_section_from_table(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.hovered_section != Some(index)
            || self.bar_cursor_x.is_some()
            || self.bar_cursor_addr.is_some()
        {
            self.hovered_section = Some(index);
            // Clear bar-source cursor data so the renderer's fallback
            // (section centre) kicks in — the table doesn't know a
            // specific address.
            self.bar_cursor_x = None;
            self.bar_cursor_addr = None;
            cx.notify();
        }
    }

    fn ensure_section_table_state(&mut self, len: usize) {
        if self.section_table_len != len {
            self.section_table_scroll =
                ListState::new(len, ListAlignment::Top, px(2000.));
            self.section_table_len = len;
        }
    }

    /// Save the current bundle's UI state to the staged-write set.
    /// The flush timer turns it into a real DB write within 500ms.
    fn save_state(&self) {
        let (Some(db), Some(bundle)) = (&self.db, self.bundle()) else { return };
        let Some(bundle_id) = bundle.bundle_id.clone() else { return };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rec = glass_db::BundleRecord {
            schema_version: 1,
            label: bundle.display_label.clone(),
            last_opened_unix: now,
            artifacts: bundle.artifact_ids.as_ref().clone(),
            open_tabs: self.tabs.iter().map(|t| t.kind.to_state()).collect(),
            active_tab: self.active_tab,
            expanded_paths: self.expanded.open.iter().cloned().collect(),
            source_path: self
                .source_path
                .as_ref()
                .and_then(|p| p.to_str().map(|s| s.to_string())),
        };
        db.save_bundle(bundle_id, rec);
    }

    /// Restore previously-saved tabs + expansion for this bundle, if any.
    fn restore_state(&mut self, bundle: &LoadedBundle) {
        let (Some(db), Some(bundle_id)) = (&self.db, bundle.bundle_id.as_ref()) else {
            return;
        };
        let rec = match db.load_bundle(bundle_id) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("glass-db read failed: {e:#}");
                return;
            }
        };
        // Tabs.
        for state in &rec.open_tabs {
            let kind = match state {
                glass_db::TabState::SmaliClass { class_jni } => {
                    TabKind::SmaliClass { class_jni: class_jni.clone() }
                }
                glass_db::TabState::Listing { artifact, section, .. } => {
                    TabKind::Listing {
                        artifact: artifact.clone(),
                        section: section.clone(),
                    }
                }
                glass_db::TabState::Hex { artifact, section, .. } => {
                    TabKind::Hex {
                        artifact: artifact.clone(),
                        section: section.clone(),
                    }
                }
                glass_db::TabState::SectionMap { artifact } => {
                    TabKind::SectionMap { artifact: artifact.clone() }
                }
                // Unknown view kinds (Symbols, Strings, Manifest) are
                // silently dropped until their runtime lands.
                _ => continue,
            };
            // Only restore tabs whose target still exists in this bundle.
            if bundle.resolve(&kind.to_state()).is_some() {
                self.tabs.push(Tab::new(kind));
            }
        }
        if let Some(idx) = rec.active_tab {
            if idx < self.tabs.len() {
                self.active_tab = Some(idx);
            }
        }
        // Expansion. We overwrite any default expansion the caller may
        // have set so the user's last state wins.
        let restored: std::collections::HashSet<Vec<usize>> =
            rec.expanded_paths.into_iter().collect();
        if !restored.is_empty() {
            self.expanded.open = restored;
        }
    }

    fn bundle(&self) -> Option<&LoadedBundle> {
        match &self.state {
            ShellState::Ready(b) => Some(b),
            _ => None,
        }
    }

    /// Resolve a tab to its current `LeafId` (which may change across
    /// bundle reloads even though the `TabKind` identity is stable).
    fn tab_leaf(&self, index: usize) -> Option<LeafId> {
        let bundle = self.bundle()?;
        let tab = self.tabs.get(index)?;
        bundle.resolve(&tab.kind.to_state())
    }

    fn active_leaf(&self) -> Option<LeafId> {
        self.active_tab.and_then(|i| self.tab_leaf(i))
    }

    /// Tab label as shown in the tab bar.
    ///
    /// We drive the label directly from `TabKind` rather than from
    /// `bundle.labels` so views that don't correspond to a tree leaf
    /// (e.g. a Listing for `.rodata`, opened via the SectionMap) still
    /// have a sensible name. `bundle.labels` is consulted only as a
    /// fallback for SmaliClass when we want the simple class name.
    ///
    /// When multiple tabs share the same `TabKind`, suffix with `#N`.
    fn tab_display_label(&self, bundle: &LoadedBundle, index: usize) -> SharedString {
        let Some(tab) = self.tabs.get(index) else {
            return SharedString::from(format!("#{}", index));
        };
        let base: SharedString = match &tab.kind {
            TabKind::Listing { section, .. } => SharedString::from(section.clone()),
            TabKind::Hex { section, .. } => SharedString::from(section.clone()),
            TabKind::SectionMap { .. } => {
                // SectionMap leaves carry a "<lib> (overview)" label
                // already; fall back to the leaf label when we can.
                self.tab_leaf(index)
                    .and_then(|LeafId(i)| bundle.labels.get(i).cloned())
                    .unwrap_or_else(|| SharedString::from("overview"))
            }
            TabKind::SmaliClass { class_jni } => self
                .tab_leaf(index)
                .and_then(|LeafId(i)| bundle.labels.get(i).cloned())
                .unwrap_or_else(|| SharedString::from(class_jni.clone())),
            TabKind::Manifest => self
                .tab_leaf(index)
                .and_then(|LeafId(i)| bundle.labels.get(i).cloned())
                .unwrap_or_else(|| SharedString::from("manifest")),
        };
        // Count tabs of the same kind. Number only when ≥2 exist.
        let total = self.tabs.iter().filter(|t| t.kind == tab.kind).count();
        if total <= 1 {
            return base;
        }
        let nth = 1 + self.tabs[..index].iter().filter(|t| t.kind == tab.kind).count();
        SharedString::from(format!("{base} #{nth}"))
    }

    fn set_tab_bar_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        // Only notify on real change to avoid an infinite re-render loop —
        // canvas writes width → notify → render → canvas writes width → ...
        if (self.tab_bar_width - width).abs() > px(0.5) {
            self.tab_bar_width = width;
            cx.notify();
        }
    }

    fn toggle_overflow(&mut self, cx: &mut Context<Self>) {
        self.overflow_open = !self.overflow_open;
        cx.notify();
    }

    /// Lazily populate the active tab's line cache. Returns `None` if
    /// there is no active tab or the bundle is gone.
    /// Spawn a worker thread that runs `build_listing_rows`, plus a
    /// foreground task that animates progress and installs the result.
    fn spawn_listing_build(
        &self,
        kind: TabKind,
        text: TextSectionBytes,
        symbols: Arc<glass_arch_arm64::SymbolMap>,
        data: Arc<DataPeek>,
        progress: Arc<Mutex<Progress>>,
        cx: &mut Context<Self>,
    ) {
        let progress_for_bg = progress.clone();
        let symbols_for_bg = symbols.clone();
        let text_for_bg = text.clone();
        let data_for_bg = data.clone();
        let build_task = cx.background_executor().spawn(async move {
            build_listing_rows(
                &text_for_bg,
                &symbols_for_bg,
                &data_for_bg,
                Some(&progress_for_bg),
            )
        });

        let progress_for_poll = progress.clone();
        cx.spawn(async move |this, cx| {
            // Animate the bar while the worker runs. Same shape as the
            // bundle-loader poll loop.
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                let _ = this.update(cx, |_s, cx| cx.notify());
                let done = progress_for_poll.lock().map(|p| p.done).unwrap_or(true);
                if done {
                    break;
                }
            }
            let rows = build_task.await;
            let comment_count = rows
                .iter()
                .filter(|r| {
                    matches!(r, ListingRow::Instruction { comment, .. } if !comment.is_empty())
                })
                .count();
            tracing::info!(
                "listing build: total_rows={}, comments={}",
                rows.len(),
                comment_count
            );
            let rows = Arc::new(rows);
            let _ = this.update(cx, |shell, cx| {
                let Some(idx) = shell.tabs.iter().position(|t| t.kind == kind) else {
                    return;
                };
                if let Some(tab) = shell.tabs.get_mut(idx) {
                    tab.scroll =
                        ListState::new(rows.len(), ListAlignment::Top, px(2000.));
                    tab.listing_rows = Some(rows.clone());
                    tab.listing_progress = None;
                    // Apply any pending scroll request now that rows exist.
                    if let Some(addr) = tab.pending_scroll_addr.take() {
                        if let Some(row_idx) = listing_row_for_addr(rows.as_ref(), addr)
                        {
                            scroll_into_view_with_context(&tab.scroll, row_idx);
                            tab.selected_row = Some(row_idx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn ensure_active_tab_lines(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(bundle) = self.bundle().cloned() else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        match &tab.kind {
            // SectionMap renders its own widget — no setup here.
            TabKind::SectionMap { .. } => {}
            // Manifest: rows are precomputed at bundle load. Just
            // size the scroll state once on first activation.
            TabKind::Manifest => {
                let len = bundle.manifest_rows.len();
                if tab.lines.is_none() {
                    tab.scroll = ListState::new(len, ListAlignment::Top, px(2000.));
                    // Reuse `lines` as a "did initial setup" marker —
                    // empty vec is enough.
                    tab.lines = Some(Arc::new(Vec::new()));
                }
            }
            // Hex: cheap to build (one row per 16 bytes), do it inline
            // on first activation.
            TabKind::Hex { artifact, section } => {
                let key = (artifact.clone(), section.clone());
                let Some(data) = bundle.data_sections.get(&key) else {
                    return;
                };
                if tab.hex_rows.is_none() {
                    let empty = glass_arch_arm64::SymbolMap::default();
                    let symbols = bundle.symbol_maps.get(artifact).unwrap_or(&empty);
                    let rows = build_hex_rows(data, symbols);
                    tab.scroll = ListState::new(rows.len(), ListAlignment::Top, px(2000.));
                    tab.hex_rows = Some(Arc::new(rows));
                }
                // Pending scroll-to address (clicked from a Listing's
                // resolved-symbol comment, future feature).
                if let Some(addr) = tab.pending_scroll_addr.take() {
                    if let Some(rows) = tab.hex_rows.as_ref() {
                        if let Some(idx) = hex_row_for_addr(rows.as_ref(), addr) {
                            scroll_into_view_with_context(&tab.scroll, idx);
                            tab.selected_row = Some(idx);
                            tab.selected_byte_addr = Some(addr);
                        }
                    }
                }
            }
            // Listing: kick off a background build the first time the
            // tab is activated. Worker thread fills in `listing_rows`;
            // a foreground poll loop animates the progress bar.
            TabKind::Listing { artifact, section } => {
                let artifact = artifact.clone();
                let section = section.clone();
                let key = (artifact.clone(), section.clone());
                let Some(text) = bundle.text_sections.get(&key).cloned() else {
                    return;
                };
                // First decide what to do based on tab state, *then* drop
                // the borrow before calling spawn_listing_build.
                let mut start_build = None;
                if tab.listing_rows.is_none() && tab.listing_progress.is_none() {
                    let empty = glass_arch_arm64::SymbolMap::default();
                    let symbols_arc: Arc<glass_arch_arm64::SymbolMap> = Arc::new(
                        bundle.symbol_maps.get(&artifact).cloned().unwrap_or(empty),
                    );
                    // Snapshot this artifact's data sections so the
                    // worker can peek string literals when forming
                    // adrp+add comments. Sharing through Arc keeps it
                    // cheap on big binaries.
                    let mut data_sections = Vec::new();
                    for ((aid, _name), ds) in bundle.data_sections.iter() {
                        if aid != &artifact {
                            continue;
                        }
                        // Skip DWARF / debug sections: they live in
                        // their own base-0 address space (when unlinked
                        // or shipped that way) and trick `peek_string`
                        // into thinking every pointer is "inside" them.
                        if ds.kind == NativeSectionKind::Debug {
                            continue;
                        }
                        if ds.base == 0 {
                            continue;
                        }
                        data_sections.push((ds.base, ds.bytes.clone()));
                    }
                    let data_arc = Arc::new(DataPeek { sections: data_sections });
                    let n = text.instruction_count();
                    let progress = Arc::new(Mutex::new(Progress {
                        label: section.clone(),
                        phase: SharedString::from("Disassembling…"),
                        current: 0,
                        total: n,
                        done: false,
                    }));
                    tab.listing_progress = Some(progress.clone());
                    let kind = tab.kind.clone();
                    start_build = Some((kind, symbols_arc, data_arc, progress));
                }
                if tab.listing_rows.is_some() {
                    if let Some(addr) = tab.pending_scroll_addr.take() {
                        if let Some(rows) = tab.listing_rows.as_ref() {
                            if let Some(idx) = listing_row_for_addr(rows.as_ref(), addr) {
                                scroll_into_view_with_context(&tab.scroll, idx);
                                tab.selected_row = Some(idx);
                            }
                        }
                    }
                }
                // `tab` borrow ends here; spawn the build outside.
                if let Some((kind, symbols_arc, data_arc, progress)) = start_build {
                    self.spawn_listing_build(
                        kind, text, symbols_arc, data_arc, progress, cx,
                    );
                }
            }
            // SmaliClass: pre-built line cache.
            TabKind::SmaliClass { .. } => {
                let Some(leaf) = self.tabs.get(active).and_then(|t| {
                    bundle.resolve(&t.kind.to_state())
                }) else {
                    return;
                };
                let tab = self.tabs.get_mut(active).unwrap();
                if tab.lines.is_none() {
                    let lines: Vec<SharedString> = bundle
                        .bodies
                        .get(leaf.0)
                        .map(|s| {
                            s.lines()
                                .map(|l| SharedString::from(l.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    tab.scroll =
                        ListState::new(lines.len(), ListAlignment::Top, px(2000.));
                    tab.lines = Some(Arc::new(lines));
                }
                // Consume any pending deep-link line target now that
                // the body's line count is known (so scroll-to clamps
                // correctly).
                if let Some(line_no) = tab.pending_smali_scroll_line.take() {
                    let len = tab.lines.as_ref().map(|v| v.len()).unwrap_or(0);
                    if line_no < len {
                        scroll_into_view_with_context(&tab.scroll, line_no);
                        tab.selected_row = Some(line_no);
                    }
                }
            }
        }
    }

    fn rebuild_list_state(&mut self) {
        let visible = self
            .bundle()
            .map(|b| flatten(&b.tree, &self.expanded).len())
            .unwrap_or(0);
        if visible != self.visible_count {
            self.list_state = ListState::new(visible, ListAlignment::Top, px(2000.));
            self.visible_count = visible;
        }
    }

    fn toggle_group(&mut self, path: Vec<usize>, cx: &mut Context<Self>) {
        self.expanded.toggle(&path);
        self.rebuild_list_state();

        // On expand: pin the just-expanded group to the top of the viewport
        // so its newly-revealed children flow down into view. ListState's
        // own bottom-clamp keeps short tail expansions from over-scrolling.
        if self.expanded.contains(&path) {
            if let Some(bundle) = self.bundle() {
                let rows = flatten(&bundle.tree, &self.expanded);
                if let Some(group_idx) = rows.iter().position(
                    |r| matches!(&r.kind, RowKind::Group { path: p, .. } if p == &path),
                ) {
                    self.list_state.scroll_to(ListOffset {
                        item_ix: group_idx,
                        offset_in_item: px(0.),
                    });
                }
            }
        }

        cx.notify();
        self.save_state();
    }

    /// Open (or focus) a Listing tab for the given (artifact, section)
    /// with an initial scroll target. If a matching tab is already open,
    /// we focus it and update its pending scroll target so the next
    /// paint jumps to the requested address.
    /// Select a row in the active tab. Idempotent. Clears any
    /// per-byte selection (hex view) so a fresh row click resets the
    /// cell highlight; cell clicks re-set the byte via `select_byte`
    /// afterwards.
    // ---- search palette ----------------------------------------------------

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.palette_open = false;
        } else {
            self.palette_open = true;
            self.palette_focused = true;
            self.refresh_palette_list();
            self.spawn_search_index_build_if_needed(cx);
            // Pull keyboard focus onto our root so typing reaches the
            // palette without the user clicking it first.
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        if self.palette_open {
            self.palette_open = false;
            cx.notify();
        }
    }

    fn palette_type(&mut self, s: &str, cx: &mut Context<Self>) {
        self.palette_query.push_str(s);
        self.palette_selected = 0;
        self.refresh_palette_list();
        cx.notify();
    }

    fn palette_backspace(&mut self, cx: &mut Context<Self>) {
        self.palette_query.pop();
        self.palette_selected = 0;
        self.refresh_palette_list();
        cx.notify();
    }

    fn palette_move(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.palette_list_len == 0 {
            return;
        }
        let max = self.palette_list_len.saturating_sub(1);
        let next = (self.palette_selected as i32 + delta).clamp(0, max as i32) as usize;
        if next != self.palette_selected {
            self.palette_selected = next;
            self.palette_list_state.scroll_to_reveal_item(next);
            cx.notify();
        }
    }

    fn palette_activate(&mut self, cx: &mut Context<Self>) {
        let Some(idx) = self.search_index.clone() else {
            return;
        };
        let cap = 50;
        let results = idx.filter(&self.palette_query, cap);
        let Some(entry) = results.get(self.palette_selected).cloned() else {
            return;
        };
        let jump = entry.jump.clone();
        self.palette_open = false;
        match jump {
            SearchJump::Listing { artifact, section, addr } => {
                self.open_listing_in_new_tab(artifact, section, addr, cx);
            }
            SearchJump::Hex { artifact, section, addr } => {
                self.open_hex_in_new_tab(artifact, section, addr, cx);
            }
            SearchJump::SmaliClass { class_jni } => {
                // Find the leaf with that class JNI and open it.
                let leaf = self.bundle().and_then(|b| {
                    b.resolve(&glass_db::TabState::SmaliClass {
                        class_jni: class_jni.clone(),
                    })
                });
                if let Some(leaf) = leaf {
                    self.open_leaf(leaf, cx);
                }
            }
            SearchJump::SectionMap { artifact } => {
                let leaf = self.bundle().and_then(|b| {
                    b.resolve(&glass_db::TabState::SectionMap {
                        artifact: artifact.clone(),
                    })
                });
                if let Some(leaf) = leaf {
                    self.open_leaf(leaf, cx);
                }
            }
        }
        cx.notify();
    }

    /// Recompute `palette_list_len` so up/down navigation knows the
    /// number of currently-displayed rows.
    fn refresh_palette_list(&mut self) {
        let len = self
            .search_index
            .as_ref()
            .map(|idx| idx.filter(&self.palette_query, 50).len())
            .unwrap_or(0);
        if len != self.palette_list_len {
            self.palette_list_state = ListState::new(len, ListAlignment::Top, px(800.));
            self.palette_list_len = len;
        }
        if self.palette_selected >= len {
            self.palette_selected = 0;
        }
    }

    /// Kick off the background index build on first palette open.
    /// Idempotent — does nothing if already built or in progress.
    fn spawn_search_index_build_if_needed(&mut self, cx: &mut Context<Self>) {
        if self.search_index.is_some() || self.search_indexing {
            return;
        }
        let Some(bundle) = self.bundle().cloned() else { return };
        self.search_indexing = true;
        let task = cx.background_executor().spawn(async move {
            build_search_index(&bundle)
        });
        cx.spawn(async move |this, cx| {
            let idx = task.await;
            let _ = this.update(cx, |shell, cx| {
                shell.search_index = Some(Arc::new(idx));
                shell.search_indexing = false;
                shell.refresh_palette_list();
                cx.notify();
            });
        })
        .detach();
    }

    fn select_active_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        let mut changed = false;
        if tab.selected_row != Some(row) {
            tab.selected_row = Some(row);
            changed = true;
        }
        if tab.selected_byte_addr.is_some() {
            tab.selected_byte_addr = None;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Hex-view: set the highlighted byte on the active tab. Caller is
    /// responsible for having set the matching row via
    /// `select_active_row` first.
    fn select_byte(&mut self, addr: u64, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        if tab.selected_byte_addr != Some(addr) {
            tab.selected_byte_addr = Some(addr);
            cx.notify();
        }
    }

    /// Add `dx` (positive scrolls right) to the active tab's horizontal
    /// offset, clamped to [0, max].
    fn scroll_h_by(&mut self, dx: Pixels, max: Pixels, cx: &mut Context<Self>) {
        let Some(active) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(active) else { return };
        let new_offset = (tab.h_offset + dx).clamp(px(0.), max);
        if (new_offset - tab.h_offset).abs() > px(0.1) {
            tab.h_offset = new_offset;
            cx.notify();
        }
    }

    /// Address-click inside a Listing tab: reuse the active tab (or
    /// match by kind if the active tab isn't a Listing), scroll to addr.
    /// Use `open_listing_in_new_tab` from tree / SectionMap clicks where
    /// the user expects a fresh tab.
    /// Open (or focus) the SmaliClass tab for `target_leaf` and scroll
    /// it so `line_no` is the selected, near-top row.
    fn goto_smali_method(
        &mut self,
        target_leaf: LeafId,
        line_no: usize,
        cx: &mut Context<Self>,
    ) {
        // Reuse the existing open_leaf path so we get tab dedupe + the
        // line-cache rebuild on first activation.
        self.open_leaf(target_leaf, cx);
        // Find the active tab (= the smali tab we just opened), set
        // the row + scroll. ensure_active_tab_lines runs on the next
        // paint via render, which builds tab.lines and sizes tab.scroll
        // — *after* that, scroll-to becomes meaningful. We schedule the
        // scroll for the next frame via a tiny defer.
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.selected_row = Some(line_no);
                tab.pending_smali_scroll_line = Some(line_no);
            }
        }
        cx.notify();
        self.save_state();
    }

    fn open_listing_at(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        self.overflow_open = false;
        let kind = TabKind::Listing {
            artifact: artifact.clone(),
            section: section.clone(),
        };
        // Prefer reusing the active tab when it's already a Listing for
        // this same section — that's the click-an-operand path.
        let active_matches = self
            .active_tab
            .and_then(|i| self.tabs.get(i))
            .map(|t| t.kind == kind)
            .unwrap_or(false);
        let idx = if active_matches {
            self.active_tab.unwrap()
        } else {
            // Otherwise pick any matching tab, else open a new one.
            match self.tabs.iter().position(|t| t.kind == kind) {
                Some(i) => i,
                None => {
                    self.tabs.push(Tab::new(kind));
                    self.tabs.len() - 1
                }
            }
        };
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.pending_scroll_addr = Some(addr);
        }
        self.active_tab = Some(idx);
        cx.notify();
        self.save_state();
    }

    /// Always open a new Listing tab for `(artifact, section)`, scroll
    /// to `addr`. Used by tree clicks and SectionMap clicks where the
    /// user explicitly wants another view.
    /// Always open a new Hex tab for `(artifact, section)`, scroll to
    /// `addr`. Same shape as `open_listing_in_new_tab` for symmetry.
    fn open_hex_in_new_tab(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        self.overflow_open = false;
        let kind = TabKind::Hex { artifact, section };
        self.tabs.push(Tab::new(kind));
        let idx = self.tabs.len() - 1;
        self.tabs[idx].pending_scroll_addr = Some(addr);
        self.active_tab = Some(idx);
        cx.notify();
        self.save_state();
    }

    fn open_listing_in_new_tab(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        self.overflow_open = false;
        let kind = TabKind::Listing { artifact, section };
        self.tabs.push(Tab::new(kind));
        let idx = self.tabs.len() - 1;
        self.tabs[idx].pending_scroll_addr = Some(addr);
        self.active_tab = Some(idx);
        cx.notify();
        self.save_state();
    }

    /// Open the tab corresponding to a tree leaf. SmaliClass + SectionMap
    /// dedupe by kind (one tab per class / one map per artifact makes
    /// sense). Listing always opens fresh — see `open_listing_in_new_tab`.
    fn open_leaf(&mut self, leaf: LeafId, cx: &mut Context<Self>) {
        self.overflow_open = false;
        let kind = {
            let Some(bundle) = self.bundle() else { return };
            let Some(kind_src) = bundle.kinds.get(leaf.0) else { return };
            TabKind::from_kind(kind_src)
        };
        // Listing leaves want a fresh tab on every click.
        if let TabKind::Listing { artifact, section } = &kind {
            let artifact = artifact.clone();
            let section = section.clone();
            // Open scrolled to the section base — no specific address.
            let base = self
                .bundle()
                .and_then(|b| b.text_sections.get(&(artifact.clone(), section.clone())))
                .map(|t| t.base)
                .unwrap_or(0);
            self.open_listing_in_new_tab(artifact, section, base, cx);
            return;
        }
        match self.tabs.iter().position(|t| t.kind == kind) {
            Some(i) => {
                if self.active_tab != Some(i) {
                    self.active_tab = Some(i);
                    cx.notify();
                    self.save_state();
                }
            }
            None => {
                self.tabs.push(Tab::new(kind));
                self.active_tab = Some(self.tabs.len() - 1);
                cx.notify();
                self.save_state();
            }
        }
    }

    fn focus_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.overflow_open = false;
        if index < self.tabs.len() && self.active_tab != Some(index) {
            self.active_tab = Some(index);
            cx.notify();
            self.save_state();
        }
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        self.active_tab = if self.tabs.is_empty() {
            None
        } else {
            // Prefer the tab now at `index` (the one that took its place);
            // if we closed the last tab, fall back to the new last.
            Some(index.min(self.tabs.len() - 1))
        };
        // Keep dropdown open only if there are still hidden tabs to show.
        cx.notify();
        self.save_state();
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = rgb(0x1e1e22);
        let panel = rgb(0x26262c);
        let border = rgb(0x36363c);
        let fg = rgb(0xd6d6d6);
        let dim = rgb(0x808088);
        let accent = rgb(0x4f7cff);

        let header_text: String = match &self.state {
            ShellState::Ready(b) => b.title.clone(),
            ShellState::Loading => self
                .progress
                .as_ref()
                .and_then(|p| p.lock().ok().map(|p| format!("Glass — Loading {}", p.label)))
                .unwrap_or_else(|| "Glass — Loading…".to_string()),
            ShellState::Error(_) => "Glass — load failed".to_string(),
            ShellState::Empty => "Glass — no bundle loaded".to_string(),
        };

        let header = div()
            .h(px(28.))
            .flex_shrink_0()
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .bg(panel)
            .text_sm()
            .text_color(dim)
            .child(div().flex_1().child(header_text))
            // Search affordance — clicking is equivalent to ⌘F.
            .child(
                div()
                    .id("palette-icon")
                    .px_3()
                    .h(px(24.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .text_sm()
                    .text_color(fg)
                    .border_1()
                    .border_color(border)
                    .hover(|s| s.bg(rgb(0x36363c)))
                    .cursor_pointer()
                    .child("Search")
                    .child(
                        div()
                            .text_xs()
                            .text_color(dim)
                            .child("⌘F"),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _ev, window, cx| {
                            this.toggle_palette(window, cx);
                        }),
                    ),
            );

        let body = match &self.state {
            ShellState::Ready(bundle) => {
                let bundle = bundle.clone();
                self.render_two_pane(bundle, cx, panel, border, fg, dim, accent)
                    .into_any_element()
            }
            ShellState::Loading => self
                .render_loading(panel, border, fg, dim, accent)
                .into_any_element(),
            ShellState::Error(msg) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(0xff8080))
                .child(format!("Load failed: {msg}"))
                .into_any_element(),
            ShellState::Empty => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(dim)
                .child("pass an .apk to `glass gui <path>`")
                .into_any_element(),
        };

        let palette_overlay: Option<gpui::AnyElement> = if self.palette_open {
            Some(
                self.render_palette(panel, border, fg, dim, accent, cx)
                    .into_any_element(),
            )
        } else {
            None
        };

        let mut root = div()
            .id("glass-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(bg)
            .text_color(fg)
            .font_family("Menlo")
            // Cmd-F toggles. Bound globally so it works whatever pane
            // has focus.
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                this.toggle_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteClose, _w, cx| {
                this.close_palette(cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteUp, _w, cx| {
                if this.palette_open {
                    this.palette_move(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PaletteDown, _w, cx| {
                if this.palette_open {
                    this.palette_move(1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PaletteActivate, _w, cx| {
                if this.palette_open {
                    this.palette_activate(cx);
                }
            }))
            // Capture printable keystrokes for the palette query when it's
            // open. gpui doesn't have a turnkey text input for arbitrary
            // unicode in this revision — this is enough for a search box.
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _w, cx| {
                if !this.palette_open {
                    return;
                }
                let k = &ev.keystroke;
                if k.key == "backspace" {
                    this.palette_backspace(cx);
                    return;
                }
                if k.modifiers.platform || k.modifiers.control || k.modifiers.alt {
                    return;
                }
                let Some(s) = k.key_char.as_deref() else { return };
                if s.is_empty() {
                    return;
                }
                this.palette_type(s, cx);
            }))
            .child(header)
            .child(body);
        if let Some(o) = palette_overlay {
            root = root.child(o);
        }
        root
    }
}

impl Shell {
    fn render_palette(
        &self,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let results: Vec<SearchEntry> = self
            .search_index
            .as_ref()
            .map(|idx| {
                idx.filter(&self.palette_query, 50)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let selected = self.palette_selected;
        let results_arc: Arc<Vec<SearchEntry>> = Arc::new(results);
        let scroll = self.palette_list_state.clone();
        let len = self.palette_list_len;
        let weak = cx.entity().downgrade();

        let status = if self.search_indexing {
            "indexing…".to_string()
        } else if self.search_index.is_none() {
            "no index".to_string()
        } else {
            format!("{} of {} matches", len, self
                .search_index
                .as_ref()
                .map(|i| i.entries.len())
                .unwrap_or(0))
        };

        let input_row = div()
            .h(px(40.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .px_3()
            .gap_3()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_color(dim)
                    .text_base()
                    .child("⌕"),
            )
            .child(
                div()
                    .flex_1()
                    .text_color(fg)
                    .text_base()
                    .font_family("Courier New")
                    .child(if self.palette_query.is_empty() {
                        SharedString::from("search symbols, classes, strings…")
                    } else {
                        SharedString::from(self.palette_query.clone())
                    }),
            )
            .child(div().text_color(dim).text_xs().child(status));

        let results_arc_for_list = results_arc.clone();
        let list_el = list(scroll, move |index, _w, _cx| {
            let Some(entry) = results_arc_for_list.get(index) else {
                return div().into_any();
            };
            let is_sel = index == selected;
            let bg = if is_sel { accent } else { rgb(0x00000000) };
            let weak = weak.clone();
            div()
                .id(("palette-row", index))
                .h(px(28.))
                .px_3()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .bg(bg)
                .cursor_pointer()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |_ev, _w, cx: &mut App| {
                        if let Some(entity) = weak.upgrade() {
                            cx.update_entity(&entity, |shell, cx| {
                                shell.palette_selected = index;
                                shell.palette_activate(cx);
                            });
                        }
                    },
                )
                .child(
                    div()
                        .w(px(20.))
                        .text_color(if is_sel { rgb(0xffffff) } else { dim })
                        .child(SharedString::from(entry.kind_glyph)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(if is_sel { rgb(0xffffff) } else { fg })
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(SharedString::from(entry.display.clone())),
                )
                .child(
                    div()
                        .max_w(px(280.))
                        .text_xs()
                        .text_color(if is_sel { rgb(0xddddee) } else { dim })
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(SharedString::from(entry.chip.clone())),
                )
                .into_any()
        })
        .flex_1();

        // Backdrop + centered card. Use `rgba()` not `rgb()` — gpui's
        // `rgb()` ignores the alpha byte and reads 0x00000088 as a
        // *blue* colour; `rgba(... aa)` is what we want. `.occlude()`
        // blocks every mouse interaction (click, hover, scroll-wheel)
        // from reaching the window underneath while the modal is up.
        div()
            .absolute()
            .inset_0()
            .bg(gpui::rgba(0x000000bb))
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    // Backdrop click closes.
                    this.close_palette(cx);
                }),
            )
            .child(
                div()
                    .id("palette-card")
                    .mt(px(80.))
                    .w(px(960.))
                    .h(px(540.))
                    .bg(panel)
                    .border_1()
                    .border_color(border)
                    .rounded_md()
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    // Eat clicks inside so the backdrop handler doesn't fire.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        |_ev, _w, cx: &mut App| {
                            cx.stop_propagation();
                        },
                    )
                    .child(input_row)
                    .child(list_el),
            )
    }

    fn render_loading(
        &self,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> impl IntoElement {
        match self.progress.as_ref() {
            Some(p) => self
                .render_progress(p, panel, border, fg, dim, accent)
                .into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(dim)
                .child("Loading…")
                .into_any_element(),
        }
    }

    fn render_progress(
        &self,
        progress: &Arc<Mutex<Progress>>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> impl IntoElement {
        let snapshot: Progress = progress
            .lock()
            .ok()
            .map(|p| p.clone())
            .unwrap_or(Progress {
                label: String::new(),
                phase: SharedString::from("Loading…"),
                current: 0,
                total: 0,
                done: false,
            });

        let phase = snapshot.phase.clone();
        let detail = if snapshot.total > 0 {
            format!("{} / {}", snapshot.current, snapshot.total)
        } else {
            String::new()
        };
        let fraction = if snapshot.total > 0 {
            (snapshot.current as f32 / snapshot.total as f32).clamp(0., 1.)
        } else {
            0.
        };

        // Indeterminate-style placeholder when there's no total yet:
        // show a half-width bar pinned at the start.
        let bar_width_percent = if snapshot.total > 0 {
            fraction * 100.
        } else {
            25.
        };

        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(fg)
                    .child(phase),
            )
            .child(
                // Track
                div()
                    .w(px(360.))
                    .h(px(6.))
                    .bg(panel)
                    .border_1()
                    .border_color(border)
                    .rounded_sm()
                    .relative()
                    .child(
                        // Fill
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .h_full()
                            .bg(accent)
                            .rounded_sm()
                            .w(gpui::relative(bar_width_percent / 100.)),
                    ),
            )
            .child(div().text_xs().text_color(dim).child(detail))
    }

    fn render_two_pane(
        &mut self,
        bundle: LoadedBundle,
        cx: &mut Context<Self>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> impl IntoElement {
        let rows: Arc<[VisibleRow]> = flatten(&bundle.tree, &self.expanded).into();
        let selected = self.active_leaf();
        let self_handle = cx.entity().downgrade();

        let left_scrollbar = list_scrollbar(&self.list_state, border, dim);
        let left = div()
            .w(px(340.))
            .h_full()
            .flex_shrink_0()
            .relative()
            .border_r_1()
            .border_color(border)
            .bg(panel)
            .child(
                div().size_full().flex().flex_col().child(
                list(self.list_state.clone(), {
                    let rows = rows.clone();
                    move |index, _window, _cx| {
                        let row = rows[index].clone();
                        let handle = self_handle.clone();
                        let indent = px(8. + row.depth as f32 * 14.);

                        let (is_selected, glyph, label, on_click_kind): (bool, &'static str, SharedString, RowAction) = match row.kind {
                            RowKind::Group { ref path, expanded, ref label } => (
                                false,
                                if expanded { "▾ " } else { "▸ " },
                                label.clone(),
                                RowAction::Toggle(path.clone()),
                            ),
                            RowKind::Leaf { leaf_id, ref label } => (
                                selected == Some(leaf_id),
                                "  ",
                                label.clone(),
                                RowAction::Select(leaf_id),
                            ),
                        };

                        let row_bg = if is_selected { accent } else { panel };
                        let row_fg = if is_selected { rgb(0xffffff) } else { fg };

                        div()
                            .h(px(22.))
                            .w_full()
                            .pl(indent)
                            .pr_3()
                            .flex()
                            .items_center()
                            .text_xs()
                            .bg(row_bg)
                            .text_color(row_fg)
                            .child(format!("{glyph}{label}"))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_event, _window, cx: &mut App| {
                                    let Some(entity) = handle.upgrade() else { return };
                                    let action = on_click_kind.clone();
                                    cx.update_entity(&entity, |shell, cx| match action {
                                        RowAction::Toggle(path) => shell.toggle_group(path, cx),
                                        RowAction::Select(id) => shell.open_leaf(id, cx),
                                    });
                                },
                            )
                            .into_any()
                    }
                })
                .flex_1(),
                ),
            )
            .child(left_scrollbar);

        self.ensure_active_tab_lines(cx);
        let (tab_bar, overflow_dropdown) =
            self.render_tab_bar(&bundle, cx, panel, border, fg, dim, accent);

        let active_kind = self
            .active_tab
            .and_then(|i| self.tabs.get(i))
            .map(|t| t.kind.clone());

        let body: gpui::AnyElement = match active_kind {
            Some(TabKind::SectionMap { artifact }) => self
                .render_section_map(&bundle, &artifact, panel, border, fg, dim, cx)
                .into_any_element(),
            Some(TabKind::Manifest) => {
                let (scroll, h_offset) = match self
                    .active_tab
                    .and_then(|i| self.tabs.get(i))
                {
                    Some(tab) => (tab.scroll.clone(), tab.h_offset),
                    None => (
                        ListState::new(0, ListAlignment::Top, px(2000.)),
                        px(0.),
                    ),
                };
                let v_scrollbar = list_scrollbar(&scroll, border, dim);
                let h_scrollbar = horizontal_scrollbar_offset(
                    h_offset,
                    px(LISTING_ROW_MIN_WIDTH),
                    border,
                    dim,
                );
                let max_h = px(LISTING_ROW_MIN_WIDTH);
                let rows = bundle.manifest_rows.clone();
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .overflow_hidden()
                            .on_scroll_wheel(cx.listener(
                                move |this, ev: &gpui::ScrollWheelEvent, _w, cx| {
                                    let dx = ev.delta.pixel_delta(px(22.)).x;
                                    if dx != px(0.) {
                                        this.scroll_h_by(-dx, max_h, cx);
                                    }
                                },
                            ))
                            .child(
                                list(scroll, move |index, _window, _cx| {
                                    let Some(row) = rows.get(index) else {
                                        return div().into_any();
                                    };
                                    let indent = px(8. + row.depth as f32 * 18.);
                                    // Outer row clips; inner gets translated
                                    // by h_offset so long lines slide left.
                                    let mut inner = div()
                                        .absolute()
                                        .top_0()
                                        .left(-h_offset)
                                        .h(px(22.))
                                        .w(px(LISTING_ROW_MIN_WIDTH))
                                        .pl(indent)
                                        .pr_3()
                                        .text_base()
                                        .font_family("Courier New")
                                        .text_color(rgb(COLOUR_PLAIN))
                                        .whitespace_nowrap()
                                        .flex()
                                        .flex_row()
                                        .items_center();
                                    for tok in row.chunks.iter() {
                                        inner = inner.child(
                                            div()
                                                .text_color(rgb(chunk_colour(tok.kind)))
                                                .whitespace_nowrap()
                                                .child(SharedString::from(tok.text.clone())),
                                        );
                                    }
                                    div()
                                        .h(px(22.))
                                        .w_full()
                                        .overflow_hidden()
                                        .relative()
                                        .child(inner)
                                        .into_any()
                                })
                                .size_full(),
                            )
                            .child(v_scrollbar),
                    )
                    .child(h_scrollbar)
                    .into_any_element()
            }
            Some(TabKind::Hex { artifact, .. }) => {
                let (rows_opt, scroll_opt, h_offset, selected_row, selected_byte) =
                    match self.active_tab.and_then(|i| self.tabs.get(i)) {
                        Some(tab) => (
                            tab.hex_rows.clone(),
                            Some(tab.scroll.clone()),
                            tab.h_offset,
                            tab.selected_row,
                            tab.selected_byte_addr,
                        ),
                        None => (None, None, px(0.), None, None),
                    };
                let scroll = scroll_opt.unwrap_or_else(|| {
                    ListState::new(0, ListAlignment::Top, px(2000.))
                });
                let v_scrollbar = list_scrollbar(&scroll, border, dim);
                let h_scrollbar = horizontal_scrollbar_offset(
                    h_offset,
                    px(HEX_ROW_MIN_WIDTH),
                    border,
                    dim,
                );
                let max_h = px(HEX_ROW_MIN_WIDTH);
                let rows = rows_opt.unwrap_or_else(|| Arc::new(Vec::new()));
                let ctx = RowCtx {
                    bundle: bundle.clone(),
                    artifact: artifact.clone(),
                    shell: cx.entity().downgrade(),
                    selected_row,
                };
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .overflow_hidden()
                            .on_scroll_wheel(cx.listener(
                                move |this, ev: &gpui::ScrollWheelEvent, _w, cx| {
                                    let dx = ev.delta.pixel_delta(px(HEX_ROW_HEIGHT)).x;
                                    if dx != px(0.) {
                                        this.scroll_h_by(-dx, max_h, cx);
                                    }
                                },
                            ))
                            .child(
                                list(scroll, move |index, _window, _cx| {
                                    let Some(row) = rows.get(index) else {
                                        return div().into_any();
                                    };
                                    render_hex_row(
                                        row,
                                        index,
                                        h_offset,
                                        Some(&ctx),
                                        selected_byte,
                                    )
                                    .into_any()
                                })
                                .size_full(),
                            )
                            .child(v_scrollbar),
                    )
                    .child(h_scrollbar)
                    .into_any_element()
            }
            Some(TabKind::Listing { artifact, .. }) => {
                let tab_view = self.active_tab.and_then(|i| self.tabs.get(i));
                let (rows_opt, progress_opt, scroll_opt, h_offset, selected_row) =
                    match tab_view {
                        Some(tab) => (
                            tab.listing_rows.clone(),
                            tab.listing_progress.clone(),
                            Some(tab.scroll.clone()),
                            tab.h_offset,
                            tab.selected_row,
                        ),
                        None => (None, None, None, px(0.), None),
                    };
                match (rows_opt, progress_opt) {
                    (Some(listing_rows), _) => {
                        let scroll = scroll_opt.unwrap_or_else(|| {
                            ListState::new(0, ListAlignment::Top, px(2000.))
                        });
                        let v_scrollbar = list_scrollbar(&scroll, border, dim);
                        let h_scrollbar = horizontal_scrollbar_offset(
                            h_offset,
                            px(LISTING_ROW_MIN_WIDTH),
                            border,
                            dim,
                        );
                        let max_h = (px(LISTING_ROW_MIN_WIDTH)).max(px(0.));
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .min_h_0()
                            .child(
                                div()
                                    .flex_1()
                                    .relative()
                                    .overflow_hidden()
                                    // Capture horizontal scroll wheel /
                                    // trackpad gestures and shift the
                                    // rows by adjusting h_offset.
                                    .on_scroll_wheel(cx.listener(
                                        move |this, ev: &gpui::ScrollWheelEvent, _w, cx| {
                                            let line_h = px(LISTING_ROW_HEIGHT);
                                            let dx = ev.delta.pixel_delta(line_h).x;
                                            if dx != px(0.) {
                                                this.scroll_h_by(-dx, max_h, cx);
                                            }
                                        },
                                    ))
                                    .child({
                                        let ctx = RowCtx {
                                            bundle: bundle.clone(),
                                            artifact: artifact.clone(),
                                            shell: cx.entity().downgrade(),
                                            selected_row,
                                        };
                                        list(scroll, move |index, _window, _cx| {
                                            let Some(row) = listing_rows.get(index)
                                            else {
                                                return div().into_any();
                                            };
                                            render_listing_row_with(
                                                row, index, h_offset, Some(&ctx),
                                            )
                                                .into_any()
                                        })
                                        .size_full()
                                    })
                                    .child(v_scrollbar),
                            )
                            .child(h_scrollbar)
                            .into_any_element()
                    }
                    (None, Some(progress)) => self
                        .render_progress(&progress, panel, border, fg, dim, accent)
                        .into_any_element(),
                    (None, None) => div().flex_1().into_any_element(),
                }
            }
            Some(TabKind::SmaliClass { .. }) | None => {
                let (right_state, right_lines, h_offset, selected_row) = match self
                    .active_tab
                    .and_then(|i| self.tabs.get(i))
                {
                    Some(tab) => (
                        tab.scroll.clone(),
                        tab.lines.clone().unwrap_or_else(|| Arc::new(Vec::new())),
                        tab.h_offset,
                        tab.selected_row,
                    ),
                    None => (
                        ListState::new(0, ListAlignment::Top, px(2000.)),
                        Arc::new(Vec::new()),
                        px(0.),
                        None,
                    ),
                };
                let shell_weak = cx.entity().downgrade();
                let v_scrollbar = list_scrollbar(&right_state, border, dim);
                let h_scrollbar = horizontal_scrollbar_offset(
                    h_offset,
                    px(LISTING_ROW_MIN_WIDTH),
                    border,
                    dim,
                );
                let max_h = px(LISTING_ROW_MIN_WIDTH);
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .overflow_hidden()
                            .on_scroll_wheel(cx.listener(
                                move |this, ev: &gpui::ScrollWheelEvent, _w, cx| {
                                    let dx = ev.delta.pixel_delta(px(22.)).x;
                                    if dx != px(0.) {
                                        this.scroll_h_by(-dx, max_h, cx);
                                    }
                                },
                            ))
                            .child(
                                list(right_state, {
                                    let lines = right_lines.clone();
                                    let shell_weak = shell_weak.clone();
                                    let bundle = bundle.clone();
                                    move |index, _window, _cx| {
                                        let text = lines
                                            .get(index)
                                            .cloned()
                                            .unwrap_or_else(|| SharedString::from(""));
                                        let is_selected =
                                            selected_row == Some(index);
                                        let mut row = div()
                                            .id(("smali-row", index))
                                            .h(px(22.))
                                            .w_full()
                                            .overflow_hidden()
                                            .relative();
                                        if is_selected {
                                            row = row.bg(rgb(COLOUR_ROW_SELECTED));
                                        }
                                        let weak = shell_weak.clone();
                                        // Tokenise the line and build a
                                        // flex-row of coloured chunks. Same
                                        // shape as the listing renderer.
                                        let tokens = tokenize_smali_line(text.as_ref());
                                        let mut inner = div()
                                            .absolute()
                                            .top_0()
                                            .left(-h_offset)
                                            .h(px(22.))
                                            .w(px(LISTING_ROW_MIN_WIDTH))
                                            .px_3()
                                            .text_base()
                                            .font_family("Courier New")
                                            .text_color(rgb(COLOUR_PLAIN))
                                            .whitespace_nowrap()
                                            .flex()
                                            .flex_row()
                                            .items_center();
                                        for (tok_idx, tok) in tokens.into_iter().enumerate() {
                                            // Class-ref Type chunks get
                                            // resolved against the bundle.
                                            // Internal classes are bright
                                            // and clickable; externals are
                                            // dimmed and inert.
                                            // MethodName chunk: render
                                            // clickable+underlined when the
                                            // `target_text` (`Class;->name(sig)ret`)
                                            // resolves to a known method line.
                                            if tok.kind == glass_arch_arm64::ChunkKind::MethodName
                                            {
                                                let key = tok.target_text.clone();
                                                let location: Option<(LeafId, usize)> = key
                                                    .as_ref()
                                                    .and_then(|k| bundle.method_lines.get(k))
                                                    .copied();
                                                let base_div = div()
                                                    .text_color(rgb(if location.is_some() {
                                                        COLOUR_PLAIN
                                                    } else {
                                                        COLOUR_PLAIN
                                                    }))
                                                    .whitespace_nowrap()
                                                    .child(SharedString::from(
                                                        tok.text.clone(),
                                                    ));
                                                if let Some((target_leaf, line_no)) = location {
                                                    let weak = weak.clone();
                                                    let tooltip_label = key
                                                        .as_ref()
                                                        .map(|s| format!("goto {s}"))
                                                        .unwrap_or_default();
                                                    let chip = base_div
                                                        .id((
                                                            "smali-method",
                                                            index * 1024 + tok_idx,
                                                        ))
                                                        .cursor_pointer()
                                                        .hover(|s| s.underline())
                                                        .on_mouse_down(
                                                            gpui::MouseButton::Left,
                                                            move |_ev, _w, cx: &mut App| {
                                                                cx.stop_propagation();
                                                                let Some(entity) =
                                                                    weak.upgrade()
                                                                else {
                                                                    return;
                                                                };
                                                                cx.update_entity(
                                                                    &entity,
                                                                    |shell, cx| {
                                                                        shell.goto_smali_method(
                                                                            target_leaf,
                                                                            line_no,
                                                                            cx,
                                                                        );
                                                                    },
                                                                );
                                                            },
                                                        )
                                                        .tooltip(move |_w, cx| {
                                                            cx.new(|_| TextTooltip {
                                                                text: SharedString::from(
                                                                    tooltip_label.clone(),
                                                                ),
                                                            })
                                                            .into()
                                                        });
                                                    inner = inner.child(chip);
                                                } else {
                                                    inner = inner.child(base_div);
                                                }
                                                continue;
                                            }
                                            if tok.kind == glass_arch_arm64::ChunkKind::Type {
                                                if let Some(jni) = extract_class_jni(&tok.text) {
                                                    let resolves = bundle
                                                        .resolve(
                                                            &glass_db::TabState::SmaliClass {
                                                                class_jni: jni.to_string(),
                                                            },
                                                        )
                                                        .is_some();
                                                    let colour = if resolves {
                                                        COLOUR_TYPE
                                                    } else {
                                                        COLOUR_TYPE_EXTERNAL
                                                    };
                                                    if resolves {
                                                        let jni = jni.to_string();
                                                        let dotted = jni_to_dotted(&jni);
                                                        let tooltip_label =
                                                            format!("goto {dotted}");
                                                        let weak = weak.clone();
                                                        let chip = div()
                                                            .id((
                                                                "smali-type",
                                                                index * 1024 + tok_idx,
                                                            ))
                                                            .text_color(rgb(colour))
                                                            .whitespace_nowrap()
                                                            .cursor_pointer()
                                                            .hover(|s| s.underline())
                                                            .child(SharedString::from(
                                                                tok.text.clone(),
                                                            ))
                                                            .on_mouse_down(
                                                                gpui::MouseButton::Left,
                                                                move |_ev, _w, cx: &mut App| {
                                                                    cx.stop_propagation();
                                                                    let Some(entity) =
                                                                        weak.upgrade()
                                                                    else {
                                                                        return;
                                                                    };
                                                                    let jni = jni.clone();
                                                                    cx.update_entity(
                                                                        &entity,
                                                                        |shell, cx| {
                                                                            if let Some(leaf) =
                                                                                shell.bundle().and_then(|b| {
                                                                                    b.resolve(
                                                                                        &glass_db::TabState::SmaliClass {
                                                                                            class_jni: jni.clone(),
                                                                                        },
                                                                                    )
                                                                                })
                                                                            {
                                                                                shell.open_leaf(
                                                                                    leaf, cx,
                                                                                );
                                                                            }
                                                                        },
                                                                    );
                                                                },
                                                            )
                                                            .tooltip(
                                                                move |_w, cx| {
                                                                    cx.new(|_| TextTooltip {
                                                                        text: SharedString::from(
                                                                            tooltip_label.clone(),
                                                                        ),
                                                                    })
                                                                    .into()
                                                                },
                                                            );
                                                        inner = inner.child(chip);
                                                        continue;
                                                    } else {
                                                        // External — render dimmed.
                                                        inner = inner.child(
                                                            div()
                                                                .text_color(rgb(colour))
                                                                .whitespace_nowrap()
                                                                .child(SharedString::from(
                                                                    tok.text,
                                                                )),
                                                        );
                                                        continue;
                                                    }
                                                }
                                            }
                                            inner = inner.child(
                                                div()
                                                    .text_color(rgb(chunk_colour(tok.kind)))
                                                    .whitespace_nowrap()
                                                    .child(SharedString::from(tok.text)),
                                            );
                                        }
                                        row.on_mouse_down(
                                            gpui::MouseButton::Left,
                                            move |_ev, _w, cx: &mut App| {
                                                if let Some(entity) = weak.upgrade() {
                                                    cx.update_entity(
                                                        &entity,
                                                        |shell, cx| {
                                                            shell.select_active_row(
                                                                index, cx,
                                                            );
                                                        },
                                                    );
                                                }
                                            },
                                        )
                                        .child(inner)
                                        .into_any()
                                    }
                                })
                                .size_full(),
                            )
                            .child(v_scrollbar),
                    )
                    .child(h_scrollbar)
                    .into_any_element()
            }
        };

        let right = div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .relative()
            .child(tab_bar)
            .child(body)
            .child(overflow_dropdown);

        div()
            .flex_1()
            .flex()
            .flex_row()
            .overflow_hidden()
            .child(left)
            .child(right)
    }

    /// Side-panel tooltip rendered to the right of the section table.
    /// Returns `None` if no section is hovered.
    fn build_section_tooltip(
        &self,
        sections: &[SectionInfo],
        bundle: &LoadedBundle,
        artifact: &glass_db::ArtifactId,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
    ) -> Option<gpui::AnyElement> {
        let idx = self.hovered_section?;
        let sec = sections.get(idx)?;
        let end = sec.address + sec.size;
        let empty = glass_arch_arm64::SymbolMap::default();
        let symbol_map = bundle.symbol_maps.get(artifact).unwrap_or(&empty);
        let in_section: Vec<&glass_arch_arm64::Symbol> =
            symbol_map.in_range(sec.address, end).collect();
        let covering = self
            .bar_cursor_addr
            .and_then(|addr| symbol_map.covering(addr));

        let mut body = div()
            .w(px(280.))
            .flex_shrink_0()
            .p_3()
            .bg(rgb(0x18181c))
            .border_1()
            .border_color(border)
            .rounded_md()
            .flex()
            .flex_col()
            .gap_1()
            .text_xs()
            .text_color(fg);

        // Header: section name + kind chip + addr range.
        body = body.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(8.)).h(px(8.)).bg(rgb(sec.kind.colour())))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(0xffffff))
                        .child(sec.name.clone()),
                )
                .child(
                    div()
                        .text_color(dim)
                        .child(SharedString::from(sec.kind.label())),
                ),
        );
        body = body.child(
            div().text_color(dim).child(format!(
                "0x{:x} – 0x{:x}   ({} bytes)",
                sec.address, end, sec.size,
            )),
        );

        // Cursor address + covering symbol.
        if let Some(addr) = self.bar_cursor_addr {
            let line = match covering {
                Some(s) => {
                    let off = addr - s.address;
                    if off == 0 {
                        format!("@ 0x{:x}   {}", addr, s.display_name)
                    } else {
                        format!("@ 0x{:x}   {} + 0x{:x}", addr, s.display_name, off)
                    }
                }
                None => format!("@ 0x{:x}", addr),
            };
            body = body.child(div().text_color(rgb(0xf2f2f2)).child(line));
        }

        // Symbol count + first few entries.
        body = body.child(
            div()
                .text_color(dim)
                .child(format!("{} symbols in section", in_section.len())),
        );
        for sym in in_section.iter().take(5) {
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .w(px(70.))
                            .text_color(dim)
                            .font_family("Courier New")
                            .child(format!("{:08x}", sym.address)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(sym.display_name.clone()),
                    ),
            );
        }
        if in_section.len() > 5 {
            body = body.child(
                div()
                    .text_color(dim)
                    .child(format!("… ({} more)", in_section.len() - 5)),
            );
        }

        Some(body.into_any_element())
    }

    fn render_section_map(
        &mut self,
        bundle: &LoadedBundle,
        artifact: &glass_db::ArtifactId,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let empty = Vec::new();
        let sections_ref: &Vec<SectionInfo> =
            bundle.native_sections.get(artifact).unwrap_or(&empty);
        let sections: Arc<Vec<SectionInfo>> = Arc::new(sections_ref.clone());
        self.ensure_section_table_state(sections.len());
        let hovered = self.hovered_section;

        // ---- coloured strip --------------------------------------------
        // Wrapped in a `relative` host so we can absolute-position a
        // canvas (for bounds measurement) and the hover cursor over it.
        let mut bar_inner = div()
            .size_full()
            .flex()
            .flex_row()
            .rounded_sm()
            .overflow_hidden();
        for (i, sec) in sections.iter().enumerate() {
            // Floor to a tiny visible share so the strip stays
            // clickable end-to-end.
            let f = sec.fraction.max(0.002);
            let is_hot = hovered == Some(i);
            let cell_bg = if is_hot {
                rgb(brighten(sec.kind.colour()))
            } else {
                rgb(sec.kind.colour())
            };
            bar_inner = bar_inner.child(
                div()
                    .h_full()
                    .w(gpui::relative(f))
                    .bg(cell_bg)
                    .border_r_1()
                    .border_color(border),
            );
        }

        // Measurement canvas — fills the bar, writes window-coordinate
        // bounds into Shell so on_mouse_move can convert positions.
        let weak = cx.entity().downgrade();
        let measure = gpui::canvas(
            {
                let weak = weak.clone();
                move |bounds, _window, cx| {
                    if let Some(entity) = weak.upgrade() {
                        cx.update_entity(&entity, |shell, cx| {
                            shell.set_section_bar_bounds(bounds, cx);
                        });
                    }
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        // Cursor line for the hovered section. Positioned by computing the
        // fractional centre of the hovered section.
        // Cursor x: prefer the actual mouse x (set by on_section_bar_move),
        // fall back to the section centre when the hover came from the
        // table and we have no mouse x. The fallback keeps the cursor
        // visible while scrubbing the table.
        let cursor = if let Some(i) = hovered {
            let bar_origin = self.section_bar_bounds.origin.x;
            let bar_width = self.section_bar_bounds.size.width;
            let cursor_left_frac = match self.bar_cursor_x {
                Some(x) if bar_width > px(0.) => {
                    ((x - bar_origin) / bar_width).clamp(0., 1.)
                }
                _ => {
                    let mut acc_before = 0.0_f32;
                    let mut width = 0.0_f32;
                    for (j, sec) in sections.iter().enumerate() {
                        let f = sec.fraction.max(0.002);
                        if j < i {
                            acc_before += f;
                        } else if j == i {
                            width = f;
                            break;
                        }
                    }
                    acc_before + width / 2.0
                }
            };
            let line = div()
                .absolute()
                .top_0()
                .h_full()
                .w(px(2.))
                .bg(rgb(0xffffff))
                .left(gpui::relative(cursor_left_frac));
            Some(line)
        } else {
            None
        };

        let sections_for_move = sections.clone();
        // Tooltip — built first so the bar can adopt it as a child.
        let tooltip = self.build_section_tooltip(&sections, bundle, artifact, border, fg, dim);

        let bar = div()
            .id("section-map-bar")
            .h(px(28.))
            .w_full()
            .flex_shrink_0()
            .relative()
            .border_1()
            .border_color(border)
            .rounded_sm()
            .child(bar_inner)
            .child(measure)
            .on_mouse_move(cx.listener({
                let sections = sections_for_move.clone();
                move |this, ev: &gpui::MouseMoveEvent, _window, cx| {
                    this.on_section_bar_move(ev.position, sections.as_ref(), cx);
                }
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener({
                    let sections = sections_for_move.clone();
                    let artifact = artifact.clone();
                    move |this, _ev: &gpui::MouseDownEvent, _window, cx| {
                        let Some(idx) = this.hovered_section else { return };
                        let Some(sec) = sections.get(idx) else { return };
                        let addr = this.bar_cursor_addr.unwrap_or(sec.address);
                        match sec.kind {
                            NativeSectionKind::Text => {
                                this.open_listing_in_new_tab(
                                    artifact.clone(),
                                    sec.name.to_string(),
                                    addr,
                                    cx,
                                );
                            }
                            NativeSectionKind::Bss => {
                                // No on-disk bytes — nothing to show.
                            }
                            _ => {
                                this.open_hex_in_new_tab(
                                    artifact.clone(),
                                    sec.name.to_string(),
                                    addr,
                                    cx,
                                );
                            }
                        }
                    }
                }),
            )
            .on_hover(cx.listener(|this, &hovered: &bool, _window, cx| {
                if !hovered {
                    this.on_section_bar_leave(cx);
                }
            }));
        let bar = match cursor {
            Some(c) => bar.child(c),
            None => bar,
        };

        // ---- legend ----------------------------------------------------
        let mut legend = div()
            .flex()
            .flex_row()
            .gap_4()
            .h(px(20.))
            .flex_shrink_0()
            .text_xs()
            .text_color(dim);
        for k in [
            NativeSectionKind::Text,
            NativeSectionKind::Rodata,
            NativeSectionKind::Data,
            NativeSectionKind::Bss,
            NativeSectionKind::Debug,
            NativeSectionKind::Other,
        ] {
            legend = legend.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(div().w(px(10.)).h(px(10.)).bg(rgb(k.colour())))
                    .child(SharedString::from(k.label())),
            );
        }

        // ---- table -----------------------------------------------------
        let header = div()
            .h(px(28.))
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .border_b_1()
            .border_color(border)
            .text_sm()
            .text_color(dim)
            .child(div().w(px(220.)).pl_3().child("name"))
            .child(div().w(px(160.)).child("address"))
            .child(div().w(px(140.)).child("size"))
            .child(div().flex_1().child("kind"));

        // Virtualized table body via `list()`. The hovered row gets an
        // accent background and the same row is scrolled into view
        // automatically by `scroll_to_reveal_item` in `on_section_bar_move`.
        let scroll_state = self.section_table_scroll.clone();
        let row_handle = cx.entity().downgrade();
        let row_artifact = artifact.clone();
        let table_list = list(scroll_state.clone(), {
            let sections = sections.clone();
            let row_handle = row_handle.clone();
            move |index, _window, _cx| {
                let sec = sections[index].clone();
                let is_hot = hovered == Some(index);
                let bg = if is_hot {
                    rgb(0x36363c)
                } else {
                    rgb(0x00000000)
                };
                let hover_handle = row_handle.clone();
                let click_handle = row_handle.clone();
                let click_artifact = row_artifact.clone();
                let click_section_name = sec.name.to_string();
                let click_section_addr = sec.address;
                let is_text = matches!(sec.kind, NativeSectionKind::Text);
                // BSS has no on-disk bytes, so Hex view has nothing to
                // show. Everything else is clickable.
                let is_hex_eligible =
                    !is_text && !matches!(sec.kind, NativeSectionKind::Bss);
                let is_clickable = is_text || is_hex_eligible;
                div()
                    .h(px(26.))
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .bg(bg)
                    .border_b_1()
                    .border_color(rgb(0x2d2d33))
                    .on_mouse_move(move |_ev, _window, cx: &mut App| {
                        if let Some(entity) = hover_handle.upgrade() {
                            cx.update_entity(&entity, |shell, cx| {
                                shell.set_hovered_section_from_table(index, cx);
                            });
                        }
                    })
                    .when(is_clickable, move |this| {
                        this.cursor_pointer().on_mouse_down(
                            gpui::MouseButton::Left,
                            move |_ev, _window, cx: &mut App| {
                                if let Some(entity) = click_handle.upgrade() {
                                    cx.update_entity(&entity, |shell, cx| {
                                        if is_text {
                                            shell.open_listing_in_new_tab(
                                                click_artifact.clone(),
                                                click_section_name.clone(),
                                                click_section_addr,
                                                cx,
                                            );
                                        } else {
                                            shell.open_hex_in_new_tab(
                                                click_artifact.clone(),
                                                click_section_name.clone(),
                                                click_section_addr,
                                                cx,
                                            );
                                        }
                                    });
                                }
                            },
                        )
                    })
                    .child(
                        div()
                            .w(px(220.))
                            .pl_3()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(sec.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(160.))
                            .text_color(rgb(0xb0b0b0))
                            .child(format!("0x{:x}", sec.address)),
                    )
                    .child(
                        div()
                            .w(px(140.))
                            .text_color(rgb(0xb0b0b0))
                            .child(format!("0x{:x}", sec.size)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(10.)).h(px(10.)).bg(rgb(sec.kind.colour())))
                            .child(SharedString::from(sec.kind.label())),
                    )
                    .into_any()
            }
        })
        .flex_1();

        let scrollbar = list_scrollbar(&scroll_state, border, dim);
        let table = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .text_sm()
            .text_color(fg)
            .font_family("Courier New")
            .child(header)
            .child(
                div()
                    .flex_1()
                    .relative()
                    .overflow_hidden()
                    .child(div().size_full().flex().flex_col().child(table_list))
                    .child(scrollbar),
            );

        // Bottom area: table on the left, tooltip on the right.
        let bottom = match tooltip {
            Some(t) => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_row()
                .gap_3()
                .child(table)
                .child(t),
            None => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_row()
                .child(table),
        };

        // Outer: padding, fixed header (bar + legend), flex-grow bottom.
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(panel)
            .child(bar)
            .child(legend)
            .child(bottom)
    }

    fn render_tab_bar(
        &self,
        bundle: &LoadedBundle,
        cx: &mut Context<Self>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> (gpui::AnyElement, gpui::AnyElement) {
        const TAB_WIDTH: f32 = 160.;
        const OVERFLOW_BTN_WIDTH: f32 = 36.;

        let handle = cx.entity().downgrade();
        let active = self.active_tab;
        let tabs = &self.tabs;
        let bar_width = self.tab_bar_width.as_f32();
        let overflow_open = self.overflow_open;

        // How many fixed-width tabs fit. If they all fit, no overflow at all.
        // Otherwise reserve a slot for the overflow button.
        let (visible_count, has_overflow) = if bar_width <= 0. || tabs.is_empty() {
            (tabs.len(), false)
        } else {
            let raw = (bar_width / TAB_WIDTH).floor() as usize;
            if raw >= tabs.len() {
                (tabs.len(), false)
            } else {
                // Slots minus the overflow button.
                let usable = ((bar_width - OVERFLOW_BTN_WIDTH) / TAB_WIDTH).floor() as usize;
                (usable.max(1), true)
            }
        };

        // Decide which tabs are visible. Always include the active one — if it
        // would be hidden, swap it into the last visible slot.
        let mut visible: Vec<usize> = (0..visible_count.min(tabs.len())).collect();
        if has_overflow {
            if let Some(active_idx) = active {
                if !visible.contains(&active_idx) && !visible.is_empty() {
                    let last = visible.len() - 1;
                    visible[last] = active_idx;
                }
            }
        }
        let visible_set: std::collections::HashSet<usize> = visible.iter().copied().collect();
        let hidden: Vec<usize> = (0..tabs.len()).filter(|i| !visible_set.contains(i)).collect();

        // Width-measurement canvas. Its prepaint hook captures bar width into
        // `Shell` so the next render can compute the layout. Sized to fill
        // the bar so its bounds == bar bounds.
        let measure_handle = handle.clone();
        let measure = gpui::canvas(
            move |bounds, _window, cx| {
                if let Some(entity) = measure_handle.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        shell.set_tab_bar_width(bounds.size.width, cx);
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let mut bar = div()
            .h(px(30.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_stretch()
            .border_b_1()
            .border_color(border)
            .bg(panel)
            .relative()
            // Measurement layer underneath the tabs.
            .child(measure);

        if tabs.is_empty() {
            bar = bar.child(
                div()
                    .px_3()
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(dim)
                    .child("Click a class on the left to open a tab"),
            );
        }

        for &index in &visible {
            bar = bar.child(self.render_tab(
                bundle, index, active == Some(index), handle.clone(), panel, border, fg, dim,
                accent,
            ));
        }

        if has_overflow {
            let hidden_count = hidden.len();
            let toggle_handle = handle.clone();
            let overflow_btn = div()
                .h_full()
                .w(px(OVERFLOW_BTN_WIDTH))
                .flex()
                .items_center()
                .justify_center()
                .border_l_1()
                .border_color(border)
                .bg(if overflow_open { rgb(0x36363c) } else { panel })
                .text_color(fg)
                .text_xs()
                .hover(|s| s.bg(rgb(0x36363c)))
                .child(format!("▾ {}", hidden_count))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |_event, _window, cx: &mut App| {
                        let Some(entity) = toggle_handle.upgrade() else { return };
                        cx.update_entity(&entity, |shell, cx| {
                            shell.toggle_overflow(cx);
                        });
                    },
                );
            bar = bar.child(overflow_btn);
        }

        let dropdown: gpui::AnyElement = if overflow_open && !hidden.is_empty() {
            self.render_overflow_dropdown(bundle, &hidden, handle, panel, border, fg, dim, accent)
                .into_any_element()
        } else {
            // Empty placeholder so the caller always has something to attach.
            div().into_any_element()
        };

        (bar.into_any_element(), dropdown)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_tab(
        &self,
        bundle: &LoadedBundle,
        index: usize,
        is_active: bool,
        handle: gpui::WeakEntity<Self>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        accent: gpui::Rgba,
    ) -> impl IntoElement {
        const TAB_WIDTH: f32 = 160.;
        let label = self.tab_display_label(bundle, index);
        let tab_bg = if is_active { accent } else { panel };
        let tab_fg = if is_active { rgb(0xffffff) } else { fg };
        let close_fg = if is_active { rgb(0xffffff) } else { dim };
        let focus_handle = handle.clone();
        let close_handle = handle.clone();

        div()
            .w(px(TAB_WIDTH))
            .h_full()
            .px_3()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .border_r_1()
            .border_color(border)
            .bg(tab_bg)
            .text_color(tab_fg)
            .text_xs()
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(label)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_event, _window, cx: &mut App| {
                            let Some(entity) = focus_handle.upgrade() else { return };
                            cx.update_entity(&entity, |shell, cx| {
                                shell.focus_tab(index, cx);
                            });
                        },
                    ),
            )
            .child(
                div()
                    .w(px(16.))
                    .h(px(16.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .text_color(close_fg)
                    .hover(|s| s.bg(rgb(0x55555c)))
                    .child("×")
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_event, _window, cx: &mut App| {
                            cx.stop_propagation();
                            let Some(entity) = close_handle.upgrade() else { return };
                            cx.update_entity(&entity, |shell, cx| {
                                shell.close_tab(index, cx);
                            });
                        },
                    ),
            )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_overflow_dropdown(
        &self,
        bundle: &LoadedBundle,
        hidden: &[usize],
        handle: gpui::WeakEntity<Self>,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        _accent: gpui::Rgba,
    ) -> impl IntoElement {
        let mut menu = div()
            .absolute()
            .top(px(30.))
            .right_0()
            .w(px(280.))
            .max_h(px(400.))
            .overflow_hidden()
            .border_1()
            .border_color(border)
            .bg(panel)
            .shadow_lg()
            .flex()
            .flex_col();

        for &index in hidden {
            let leaf = self.tab_leaf(index);
            let label = self.tab_display_label(bundle, index);
            let origin = leaf
                .and_then(|LeafId(i)| bundle.origins.get(i).cloned())
                .unwrap_or_else(|| SharedString::from(""));

            let focus_handle = handle.clone();
            let close_handle = handle.clone();

            menu = menu.child(
                div()
                    .h(px(28.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(fg)
                    .hover(|s| s.bg(rgb(0x36363c)))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .flex()
                            .gap_2()
                            .child(label)
                            .child(div().text_color(dim).child(origin))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_event, _window, cx: &mut App| {
                                    let Some(entity) = focus_handle.upgrade() else { return };
                                    cx.update_entity(&entity, |shell, cx| {
                                        shell.focus_tab(index, cx);
                                    });
                                },
                            ),
                    )
                    .child(
                        div()
                            .w(px(16.))
                            .h(px(16.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .text_color(dim)
                            .hover(|s| s.bg(rgb(0x55555c)))
                            .child("×")
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                move |_event, _window, cx: &mut App| {
                                    cx.stop_propagation();
                                    let Some(entity) = close_handle.upgrade() else { return };
                                    cx.update_entity(&entity, |shell, cx| {
                                        shell.close_tab(index, cx);
                                    });
                                },
                            ),
                    ),
            );
        }

        menu
    }
}

#[derive(Clone)]
enum RowAction {
    Toggle(Vec<usize>),
    Select(LeafId),
}

// ---- scrollbars -------------------------------------------------------------
//
// Non-interactive (visual only) for now. Mouse-wheel scrolling works because
// the underlying scrollable elements handle it; clicking / dragging the thumb
// is a follow-up. See gpui's data_table.rs for the drag pattern.

fn list_scrollbar(state: &ListState, border: gpui::Rgba, thumb: gpui::Rgba) -> impl IntoElement {
    let max_offset = state.max_offset_for_scrollbar().y;
    let current = -state.scroll_px_offset_for_scrollbar().y;
    let viewport = state.viewport_bounds().size.height;
    track_and_thumb(viewport, max_offset, current, border, thumb)
}

/// Horizontal scrollbar driven by an explicit `h_offset` (managed by
/// the Shell). We don't know the viewport width without a layout-time
/// hook, so we approximate using the panel size we have at render time
/// (`content_width - h_offset` as an upper bound). Good enough to give
/// the user a visible position indicator that updates as they scroll.
fn horizontal_scrollbar_offset(
    h_offset: Pixels,
    content_width: Pixels,
    border: gpui::Rgba,
    thumb: gpui::Rgba,
) -> impl IntoElement {
    // We don't have access to the live viewport width here. Use a
    // pessimistic guess: assume the user sees roughly half the content
    // width at a time, so max scroll = content_width / 2. Real
    // measurement would require a canvas hook similar to the section
    // bar's bounds capture.
    let viewport = px(content_width.as_f32() / 2.);
    let max_offset = (content_width - viewport).max(px(0.));
    let current = h_offset.clamp(px(0.), max_offset);

    if max_offset <= px(0.) || viewport <= px(0.) {
        return div()
            .h(px(SCROLLBAR_WIDTH))
            .w_full()
            .flex_shrink_0();
    }

    let total = viewport + max_offset;
    let thumb_w_raw = viewport.as_f32() * viewport.as_f32() / total.as_f32();
    let thumb_w = px(thumb_w_raw.max(SCROLLBAR_MIN_THUMB));
    let fraction = (current / max_offset).clamp(0., 1.);
    let track_space = (viewport - thumb_w).max(px(0.));
    let thumb_left = track_space * fraction;

    div()
        .h(px(SCROLLBAR_WIDTH))
        .w_full()
        .flex_shrink_0()
        .border_t_1()
        .border_color(border)
        .relative()
        .child(
            div()
                .absolute()
                .left(thumb_left)
                .top(px(2.))
                .h(px(SCROLLBAR_WIDTH - 4.))
                .w(thumb_w)
                .bg(thumb)
                .rounded_sm(),
        )
}

fn track_and_thumb(
    viewport: Pixels,
    max_offset: Pixels,
    current: Pixels,
    border: gpui::Rgba,
    thumb: gpui::Rgba,
) -> impl IntoElement {
    // Hide entirely when nothing to scroll.
    if max_offset <= px(0.) || viewport <= px(0.) {
        return div()
            .absolute()
            .top_0()
            .right_0()
            .w(px(SCROLLBAR_WIDTH))
            .h_full();
    }

    let total = viewport + max_offset;
    let thumb_h_raw = viewport.as_f32() * viewport.as_f32() / total.as_f32();
    let thumb_h = px(thumb_h_raw.max(SCROLLBAR_MIN_THUMB));
    let fraction = (current / max_offset).clamp(0., 1.);
    let track_space = (viewport - thumb_h).max(px(0.));
    let thumb_top = track_space * fraction;

    div()
        .absolute()
        .top_0()
        .right_0()
        .w(px(SCROLLBAR_WIDTH))
        .h_full()
        .border_l_1()
        .border_color(border)
        .child(
            div()
                .absolute()
                .top(thumb_top)
                .left(px(2.))
                .w(px(SCROLLBAR_WIDTH - 4.))
                .h(thumb_h)
                .bg(thumb)
                .rounded_sm(),
        )
}
