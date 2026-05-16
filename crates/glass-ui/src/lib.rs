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
        /// Control-flow arrow segments this row contributes to the
        /// listing gutter. Empty for rows not touched by any in-
        /// function branch arrow.
        arrows: Arc<Vec<ArrowSegment>>,
    },
    /// Horizontal rule drawn after a basic-block terminator. Carries
    /// any arrow segments that pass over it so the control-flow lines
    /// remain continuous across BB boundaries.
    BasicBlockSeparator {
        arrows: Arc<Vec<ArrowSegment>>,
    },
}

/// One arrow segment in a listing row's gutter. Each direct branch
/// (B, B.cond, Cbz/Cbnz, Tbz/Tbnz) inside the current function gets
/// assigned a lane; every row between source and target gets the
/// segments needed to draw a continuous line from source → target →
/// arrowhead in that lane.
#[derive(Clone, Debug)]
pub struct ArrowSegment {
    /// 0 = column closest to the address text; larger = further left.
    pub lane: u8,
    /// Solid for unconditional `B`, dotted for conditionals.
    pub style: ArrowStyle,
    /// Where in this row's gutter cell the segment lives.
    pub role: ArrowRole,
    /// Down for forward branches (target is below source in row order);
    /// Up for backward branches. Affects which side of the row the
    /// horizontal stub points and which way the arrowhead faces.
    pub direction: ArrowDirection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowStyle { Solid, Dotted }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowDirection { Down, Up }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowRole {
    /// Source row: horizontal stub from the address column to the lane,
    /// plus a half-height vertical segment heading toward the target.
    Source,
    /// Target row: half-height vertical segment ending at the row
    /// middle, plus a horizontal stub with an arrowhead pointing into
    /// the address column.
    Target,
    /// Row strictly between source and target — full-height vertical.
    Pass,
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
            arrows: Arc::new(Vec::new()),
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
            rows.push(ListingRow::BasicBlockSeparator {
                arrows: Arc::new(Vec::new()),
            });
        }
    }

    assign_arrows(&mut rows);

    if let Some(p) = progress {
        if let Ok(mut p) = p.lock() {
            p.current = n;
            p.done = true;
        }
    }
    rows
}

/// After rows are built, scan every Instruction for a direct branch
/// whose target lies inside the same function and attach `ArrowSegment`s
/// to source / target / passing rows. Functions are delimited by
/// `SymbolHeader` rows (between any two consecutive headers).
///
/// Arrows are assigned lanes by a tiny sweepline so simultaneously-
/// active arrows don't visually merge. Lane 0 is closest to the
/// address column; higher lanes sit further left.
fn assign_arrows(rows: &mut [ListingRow]) {
    use glass_arch_arm64::format as fmt;
    // Build address → row-index lookup, and segment the rows into
    // [start, end) function ranges using SymbolHeader positions.
    let mut addr_to_row: std::collections::HashMap<u64, usize> =
        std::collections::HashMap::with_capacity(rows.len());
    let mut header_rows: Vec<usize> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match r {
            ListingRow::SymbolHeader { .. } => header_rows.push(i),
            ListingRow::Instruction { address, .. } => {
                addr_to_row.insert(*address, i);
            }
            _ => {}
        }
    }
    // Function ranges: [headers[k], headers[k+1]), and the prefix
    // before the first header (if any) and the suffix after the last.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut prev = 0usize;
    for &h in &header_rows {
        if h > prev {
            ranges.push((prev, h));
        }
        prev = h;
    }
    if prev < rows.len() {
        ranges.push((prev, rows.len()));
    }

    // Collect candidate arrows per function.
    #[derive(Clone)]
    struct PendingArrow {
        src_row: usize,
        tgt_row: usize,
        style: ArrowStyle,
    }
    let mut pending: Vec<PendingArrow> = Vec::new();
    for (lo, hi) in &ranges {
        for src_row in *lo..*hi {
            let ListingRow::Instruction { address: _, bytes, .. } = &rows[src_row] else {
                continue;
            };
            let word = u32::from_le_bytes(*bytes);
            // Re-decode to avoid storing the mnemonic on every row.
            // Branches are sparse — cost is negligible.
            let addr_of_row = if let ListingRow::Instruction { address, .. } = &rows[src_row] {
                *address
            } else {
                continue;
            };
            let Ok(insn) = armv8_encode::isa::aarch64::decode_instruction(addr_of_row, word)
            else {
                continue;
            };
            let style = if fmt::is_unconditional_direct_branch(insn.mnemonic) {
                ArrowStyle::Solid
            } else if fmt::is_conditional_branch(insn.mnemonic) {
                ArrowStyle::Dotted
            } else {
                continue;
            };
            let Some(target) = fmt::primary_address_operand(&insn) else { continue };
            let Some(&tgt_row) = addr_to_row.get(&target) else { continue };
            // "Within the function" — both endpoints inside the same
            // [lo, hi) range. Target row must be an Instruction (not a
            // separator) in that span. Since we only inserted
            // Instruction rows into addr_to_row, the second condition
            // is automatic; we just check the range.
            if tgt_row < *lo || tgt_row >= *hi {
                continue;
            }
            if tgt_row == src_row {
                continue;
            }
            pending.push(PendingArrow { src_row, tgt_row, style });
        }
    }

    // Lane assignment: sweepline. Sort by source row, then assign each
    // arrow the lowest lane whose previous occupant has already ended.
    pending.sort_by_key(|a| a.src_row);
    let mut lane_free_at: Vec<usize> = Vec::new(); // lane_free_at[lane] = first row index that lane is free
    for a in &pending {
        let (lo, hi) = if a.src_row <= a.tgt_row {
            (a.src_row, a.tgt_row)
        } else {
            (a.tgt_row, a.src_row)
        };
        // Find a free lane.
        let mut lane = None;
        for (idx, free_at) in lane_free_at.iter_mut().enumerate() {
            if *free_at <= lo {
                lane = Some(idx);
                *free_at = hi + 1;
                break;
            }
        }
        let lane = match lane {
            Some(l) => l,
            None => {
                lane_free_at.push(hi + 1);
                lane_free_at.len() - 1
            }
        };
        // Drop arrows that would overflow the visible gutter rather
        // than draw them clipped or off-screen.
        if (lane as u8) >= ARROW_MAX_LANES {
            continue;
        }
        let dir = if a.src_row < a.tgt_row {
            ArrowDirection::Down
        } else {
            ArrowDirection::Up
        };
        // Emit segments. We mutate `rows[row]` directly — `arrows` is
        // Arc<Vec<_>> so make_mut to clone-on-write into our owned copy.
        let push_seg = |rows: &mut [ListingRow], row: usize, role: ArrowRole| {
            let seg = ArrowSegment {
                lane: lane as u8,
                style: a.style,
                role,
                direction: dir,
            };
            match &mut rows[row] {
                ListingRow::Instruction { arrows, .. } => {
                    Arc::make_mut(arrows).push(seg);
                }
                ListingRow::BasicBlockSeparator { arrows } => {
                    // BB separators only ever host pass-through
                    // segments (the line continues over them). Force
                    // the role so a row that happens to coincide with
                    // a separator still draws a clean vertical.
                    let mut pass = seg;
                    pass.role = ArrowRole::Pass;
                    Arc::make_mut(arrows).push(pass);
                }
                _ => {}
            }
        };
        push_seg(rows, a.src_row, ArrowRole::Source);
        push_seg(rows, a.tgt_row, ArrowRole::Target);
        let (mid_lo, mid_hi) = if a.src_row < a.tgt_row {
            (a.src_row + 1, a.tgt_row)
        } else {
            (a.tgt_row + 1, a.src_row)
        };
        for r in mid_lo..mid_hi {
            push_seg(rows, r, ArrowRole::Pass);
        }
    }
}

// ---- CFG support ------------------------------------------------------------

/// Build a CFG without holding a full `Container`. We have the
/// per-artifact text-section bytes on `LoadedBundle` (used by the
/// linear-listing builder); look up which text section covers
/// `entry_addr` and delegate to armv8-encode's bytes-based CFG
/// builder.
fn build_cfg_from_text_sections(
    text_sections: &std::collections::HashMap<
        (glass_db::ArtifactId, String),
        TextSectionBytes,
    >,
    symbols: &glass_arch_arm64::SymbolMap,
    artifact: &glass_db::ArtifactId,
    entry_addr: u64,
) -> Option<glass_arch_arm64::FunctionCfg> {
    for ((aid, _name), section) in text_sections {
        if aid != artifact {
            continue;
        }
        let end = section.base + section.bytes.len() as u64;
        if entry_addr >= section.base && entry_addr < end {
            return glass_arch_arm64::build_function_cfg_from_bytes(
                section.base,
                &section.bytes,
                symbols,
                entry_addr,
            );
        }
    }
    None
}

/// Lowest LOD: render a basic block as a coloured pill with no
/// instruction text. Used when the block's on-screen width is below
/// `LOD_PILL_MAX` pixels.
/// Background fill for a normal block. Exits (`ret` / outside-fn
/// branches) get a warm tint so they stand out at low zoom.
fn cfg_block_bg(block: &glass_arch_arm64::BasicBlock) -> gpui::Rgba {
    if block.exits_function {
        gpui::rgba(0x3a2c2cff)
    } else {
        gpui::rgba(0x2a313cff)
    }
}

const CFG_BLOCK_BORDER: u32 = 0x6b6b78;

/// Pre-resolved presentational info for a CFG block. Computed once
/// per block by `render_cfg`; the LOD-specific render fns consume it.
struct CfgBlockSummary {
    /// Demangled symbol name when this address starts a named
    /// symbol — typically only the function-entry block.
    symbol: Option<SharedString>,
    /// Map from call-site instruction address to the resolved
    /// `(callee_entry_addr, display_name)`. Used by the renderer to
    /// turn `bl 0x100005814` into `bl GameMain::init` with the name
    /// clickable.
    calls: std::collections::HashMap<u64, (u64, SharedString)>,
}

/// What a CFG block should render given the current pixel budget.
/// Picked by `plan_layout` in `render_cfg` and consumed by
/// `render_cfg_block_content` so sizing + rendering agree exactly.
#[derive(Clone, Copy)]
struct CfgLayoutPlan {
    /// Number of preview rows shown at the top of the block.
    preview: usize,
    /// True when a `… <N> instructions` divider line is shown.
    show_ellipsis: bool,
    /// True when the last instruction is shown after the divider.
    /// False only if even that doesn't fit at the current zoom.
    show_last: bool,
}

fn render_cfg_block_pill(
    block: &glass_arch_arm64::BasicBlock,
    summary: &CfgBlockSummary,
    dim: gpui::Rgba,
) -> gpui::AnyElement {
    // Use the symbol when we have one (entry block), else the
    // address. Either way it's one centred line.
    let label = summary
        .symbol
        .clone()
        .unwrap_or_else(|| SharedString::from(format!("{:#x}", block.start_addr)));
    div()
        .size_full()
        .bg(cfg_block_bg(block))
        .border_2()
        .border_color(rgb(CFG_BLOCK_BORDER))
        .rounded_sm()
        .flex()
        .items_center()
        .justify_center()
        .text_color(dim)
        .text_xs()
        .font_family("Menlo")
        .child(label)
        .into_any_element()
}

/// Mid + high LOD share the same content: optional symbol header
/// (yellow, wraps if long), address, first three full instructions
/// (mnemonic + operands), and a "more instructions" footer.
/// Render context for CFG block content. Carries the bits the
/// renderer needs to wire call-target clicks back to the shell.
struct CfgBlockRenderCtx {
    shell: gpui::WeakEntity<Shell>,
    artifact: glass_db::ArtifactId,
    /// Block index — used by gpui's id() to keep stateful elements
    /// (per-row click handlers) distinct across blocks.
    block_idx: usize,
}

fn render_cfg_block_content(
    block: &glass_arch_arm64::BasicBlock,
    summary: &CfgBlockSummary,
    plan: CfgLayoutPlan,
    ctx: Option<&CfgBlockRenderCtx>,
) -> gpui::AnyElement {
    let mut body = div()
        .flex()
        .flex_col()
        .size_full()
        .px_2()
        .py_1()
        .text_xs()
        .font_family("Courier New");

    if let Some(name) = summary.symbol.as_ref() {
        body = body.child(
            div()
                .text_color(rgb(COLOUR_SYMBOL_HEADER))
                .child(SharedString::from(format!("{name}:"))),
        );
    }
    let total = block.instructions.len();
    let render_insn = |insn: &glass_arch_arm64::InstructionEntry,
                       insn_idx: usize|
     -> gpui::AnyElement {
        let mut row = div().flex().flex_row().gap_2().whitespace_nowrap();
        row = row.child(
            div()
                .text_color(rgb(COLOUR_ADDR))
                .child(SharedString::from(format!("{:016x}", insn.address))),
        );
        row = row.child(
            div()
                .text_color(rgb(COLOUR_MNEMONIC))
                .child(SharedString::from(insn.mnemonic.clone())),
        );
        // If this is a call whose target resolved to a known
        // symbol, render the symbol name (highlighted) in place of
        // the raw operand text and make it clickable to open the
        // callee's CFG.
        let call = summary.calls.get(&insn.address);
        if let Some((entry_addr, name)) = call {
            let entry_addr = *entry_addr;
            let name = name.clone();
            let label = SharedString::from(name);
            let elem: gpui::AnyElement = match ctx {
                Some(c) => {
                    let weak = c.shell.clone();
                    let artifact = c.artifact.clone();
                    div()
                        .id((
                            "cfg-call",
                            c.block_idx * 1024 + insn_idx,
                        ))
                        .text_color(rgb(COLOUR_ADDRESS_OP))
                        .cursor_pointer()
                        .hover(|s| s.bg(gpui::rgba(0xffffff20)))
                        .child(label)
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            move |_ev, _w, cx: &mut App| {
                                cx.stop_propagation();
                                if let Some(entity) = weak.upgrade() {
                                    let artifact = artifact.clone();
                                    cx.update_entity(&entity, |shell, cx| {
                                        shell.show_cfg(
                                            artifact,
                                            entry_addr,
                                            SharedString::from(""),
                                            cx,
                                        );
                                    });
                                }
                            },
                        )
                        .into_any_element()
                }
                None => div()
                    .text_color(rgb(COLOUR_ADDRESS_OP))
                    .child(label)
                    .into_any_element(),
            };
            row = row.child(elem);
        } else if !insn.operands.is_empty() {
            row = row.child(
                div()
                    .text_color(rgb(COLOUR_REGISTER))
                    .child(SharedString::from(insn.operands.clone())),
            );
        }
        row.into_any_element()
    };
    for (i, insn) in block.instructions.iter().take(plan.preview).enumerate() {
        body = body.child(render_insn(insn, i));
    }
    if plan.show_ellipsis {
        let skipped = total
            .saturating_sub(plan.preview)
            .saturating_sub(if plan.show_last { 1 } else { 0 });
        body = body.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    div()
                        .text_color(rgb(COLOUR_PUNCT))
                        .text_lg()
                        .child(SharedString::from("…")),
                )
                .child(
                    div()
                        .text_color(rgb(COLOUR_BYTES))
                        .child(SharedString::from(format!("{skipped} instructions"))),
                ),
        );
    }
    if plan.show_last {
        if let Some(last) = block.instructions.last() {
            // Use a stable per-block "last" insn slot so the gpui
            // id for the click handler stays unique. preview can be
            // 0..total-1; use total as the slot for "last".
            body = body.child(render_insn(last, total.saturating_sub(1)));
        }
    }
    if total == 0 {
        body = body.child(
            div()
                .text_color(rgb(COLOUR_BYTES))
                .child(SharedString::from("(empty)")),
        );
    }

    div()
        .size_full()
        .bg(cfg_block_bg(block))
        .border_2()
        .border_color(rgb(CFG_BLOCK_BORDER))
        .rounded_sm()
        .overflow_hidden()
        .child(body)
        .into_any_element()
}

// ---- Edge routing -----------------------------------------------------------

/// A horizontal or vertical line segment in screen-local pixel
/// coordinates. The renderer uses these for both straight strokes
/// (1 px-thick rectangles) and dotted segments (a row of small
/// rectangles).
struct EdgeSegment {
    x: f32,
    y: f32,
    /// Length along the segment's axis.
    length: f32,
    /// True for horizontal, false for vertical.
    horizontal: bool,
}


const CFG_EDGE_THICKNESS: f32 = 2.;
const CFG_EDGE_COLOR_SOLID: u32 = 0x9aa3b3;
const CFG_EDGE_COLOR_DOTTED: u32 = 0x6e7382;
const CFG_DOT_LEN: f32 = 4.;
const CFG_DOT_GAP: f32 = 3.;

fn render_edge_segment(seg: EdgeSegment, dotted: bool) -> gpui::Div {
    if dotted {
        // Compose a dotted line from short rectangles. Stride =
        // CFG_DOT_LEN + CFG_DOT_GAP. We approximate dashing by
        // stacking child divs inside a positioned container.
        let mut wrapper = div()
            .absolute()
            .left(px(seg.x - CFG_EDGE_THICKNESS / 2.))
            .top(px(seg.y - CFG_EDGE_THICKNESS / 2.));
        let stride = CFG_DOT_LEN + CFG_DOT_GAP;
        let mut pos = 0.0_f32;
        let length = seg.length;
        let colour = gpui::rgba((CFG_EDGE_COLOR_DOTTED << 8) | 0xee);
        if seg.horizontal {
            wrapper = wrapper
                .w(px(length + CFG_EDGE_THICKNESS))
                .h(px(CFG_EDGE_THICKNESS));
            while pos < length {
                let len = CFG_DOT_LEN.min(length - pos);
                wrapper = wrapper.child(
                    div()
                        .absolute()
                        .left(px(pos))
                        .top(px(0.))
                        .w(px(len))
                        .h(px(CFG_EDGE_THICKNESS))
                        .bg(colour),
                );
                pos += stride;
            }
        } else {
            wrapper = wrapper
                .w(px(CFG_EDGE_THICKNESS))
                .h(px(length + CFG_EDGE_THICKNESS));
            while pos < length {
                let len = CFG_DOT_LEN.min(length - pos);
                wrapper = wrapper.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px(pos))
                        .w(px(CFG_EDGE_THICKNESS))
                        .h(px(len))
                        .bg(colour),
                );
                pos += stride;
            }
        }
        wrapper
    } else {
        let colour = gpui::rgba((CFG_EDGE_COLOR_SOLID << 8) | 0xee);
        if seg.horizontal {
            div()
                .absolute()
                .left(px(seg.x))
                .top(px(seg.y - CFG_EDGE_THICKNESS / 2.))
                .w(px(seg.length))
                .h(px(CFG_EDGE_THICKNESS))
                .bg(colour)
        } else {
            div()
                .absolute()
                .left(px(seg.x - CFG_EDGE_THICKNESS / 2.))
                .top(px(seg.y))
                .w(px(CFG_EDGE_THICKNESS))
                .h(px(seg.length))
                .bg(colour)
        }
    }
}

/// Cardinal direction the arrowhead points in.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // Up is unused today — kept for symmetry with Down.
enum ArrowHeadDir {
    Down,
    Up,
    Left,
    Right,
}

/// Filled triangular arrowhead anchored at the tip `(tip_x, tip_y)`.
/// `dir` chooses which direction the apex points; the wedge is built
/// from stacked 1-px-wide / 1-px-tall bars whose lengths shrink with
/// the perpendicular distance from the centre line.
fn render_edge_arrowhead(
    tip_x: f32,
    tip_y: f32,
    dir: ArrowHeadDir,
) -> gpui::Div {
    const HEAD_HALF: f32 = 5.;
    const HEAD_LEN: f32 = 7.;
    let colour = gpui::rgba((CFG_EDGE_COLOR_SOLID << 8) | 0xee);

    // Compute the container's top-left + size based on `dir`. For
    // vertical arrows (Down/Up) the container is HEAD_HALF*2 wide
    // and HEAD_LEN tall. For horizontal arrows it's HEAD_LEN wide
    // and HEAD_HALF*2 tall.
    let (left, top, w, h) = match dir {
        ArrowHeadDir::Down => {
            (tip_x - HEAD_HALF, tip_y - HEAD_LEN, HEAD_HALF * 2., HEAD_LEN)
        }
        ArrowHeadDir::Up => (tip_x - HEAD_HALF, tip_y, HEAD_HALF * 2., HEAD_LEN),
        ArrowHeadDir::Left => (tip_x, tip_y - HEAD_HALF, HEAD_LEN, HEAD_HALF * 2.),
        ArrowHeadDir::Right => {
            (tip_x - HEAD_LEN, tip_y - HEAD_HALF, HEAD_LEN, HEAD_HALF * 2.)
        }
    };

    let mut head = div().absolute().left(px(left)).top(px(top)).w(px(w)).h(px(h));
    let half = HEAD_HALF as i32;
    for k in -half..=half {
        let abs_k = k.unsigned_abs() as f32;
        let bar_len = HEAD_LEN * (1.0 - abs_k / (half as f32));
        if bar_len <= 0. {
            continue;
        }
        match dir {
            ArrowHeadDir::Down => {
                // Vertical bars stacked side-by-side, all starting
                // at y=0 (base at top, apex at bottom).
                let bar_left = (k as f32) + HEAD_HALF - 0.5;
                head = head.child(
                    div()
                        .absolute()
                        .left(px(bar_left))
                        .top(px(0.))
                        .w(px(1.))
                        .h(px(bar_len))
                        .bg(colour),
                );
            }
            ArrowHeadDir::Up => {
                // Same but bars anchored at the bottom (so they grow
                // upward from y = HEAD_LEN towards y=0).
                let bar_left = (k as f32) + HEAD_HALF - 0.5;
                head = head.child(
                    div()
                        .absolute()
                        .left(px(bar_left))
                        .top(px(HEAD_LEN - bar_len))
                        .w(px(1.))
                        .h(px(bar_len))
                        .bg(colour),
                );
            }
            ArrowHeadDir::Right => {
                // Horizontal bars stacked vertically, anchored at
                // the left edge (base at left, apex at right).
                let bar_top = (k as f32) + HEAD_HALF - 0.5;
                head = head.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px(bar_top))
                        .w(px(bar_len))
                        .h(px(1.))
                        .bg(colour),
                );
            }
            ArrowHeadDir::Left => {
                // Anchored at the right edge.
                let bar_top = (k as f32) + HEAD_HALF - 0.5;
                head = head.child(
                    div()
                        .absolute()
                        .left(px(HEAD_LEN - bar_len))
                        .top(px(bar_top))
                        .w(px(bar_len))
                        .h(px(1.))
                        .bg(colour),
                );
            }
        }
    }
    head
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
const BB_SEPARATOR_HEIGHT: f32 = 8.;
const LISTING_GUTTER_WIDTH: f32 = 56.;
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
    h_shift_with_addr(inner, h_offset, row_height, row_index, ctx, None)
}

/// Same as `h_shift` but also wires a right-click context-menu opener
/// when `row_addr` is known. Used by Instruction rows so right-click
/// can resolve the covering function for "Show CFG".
fn h_shift_with_addr(
    inner: gpui::Div,
    h_offset: Pixels,
    row_height: f32,
    row_index: usize,
    ctx: Option<&RowCtx>,
    row_addr: Option<u64>,
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
        if let Some(addr) = row_addr {
            let weak = ctx.shell.clone();
            let artifact = ctx.artifact.clone();
            outer = outer.on_mouse_down(
                gpui::MouseButton::Right,
                move |ev: &gpui::MouseDownEvent, _w, cx: &mut App| {
                    if let Some(entity) = weak.upgrade() {
                        let pos = ev.position;
                        let artifact = artifact.clone();
                        cx.update_entity(&entity, |shell, cx| {
                            shell.open_listing_context_menu(artifact, addr, pos, cx);
                        });
                    }
                },
            );
        }
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

const ARROW_LANE_SPACING: f32 = 8.;
/// Distance from the gutter's right edge (= address column) to lane 0.
/// Needs to accommodate the arrowhead plus a visible horizontal turn,
/// otherwise source stubs vanish and target heads kiss the address.
const ARROW_LANE_RIGHT_MARGIN: f32 = 12.;
const ARROW_THICKNESS: f32 = 2.;
/// Arrowhead is a right-pointing wedge `ARROW_HEAD_LEN` long and
/// `ARROW_HEAD_HALF * 2 + 1` tall, built from a stack of 1 px bars.
const ARROW_HEAD_LEN: f32 = 6.;
const ARROW_HEAD_HALF: f32 = 4.;
/// Visible-lane cap. Gutter (56) minus right margin (12) divided by
/// lane spacing (8) ≈ 5.5 — round down to 5 to leave a thin breathing
/// margin on the left edge.
const ARROW_MAX_LANES: u8 = 5;

fn lane_x(lane: u8) -> f32 {
    // lane 0 sits just inside the gutter's right edge; higher lanes
    // step left in `ARROW_LANE_SPACING` increments.
    LISTING_GUTTER_WIDTH - ARROW_LANE_RIGHT_MARGIN - (lane as f32) * ARROW_LANE_SPACING
}

/// Build the gutter cell for a listing row — a fixed-width container
/// hosting absolute-positioned arrow segments. `row_h` lets BB
/// separators (which are only ~8 px tall) draw a continuous vertical
/// span instead of leaving a gap.
fn render_arrow_gutter(arrows: &Arc<Vec<ArrowSegment>>, row_h: f32) -> gpui::Div {
    let mut gutter = div()
        .w(px(LISTING_GUTTER_WIDTH))
        .h_full()
        .flex_shrink_0()
        .relative();
    if arrows.is_empty() {
        return gutter;
    }
    let mid = (row_h / 2.).floor();
    // Match the encoded-bytes column colour so arrows stay low-key.
    // Conditionals are rendered at reduced opacity since gpui doesn't
    // expose stroke dashes for divs.
    let colour_solid = gpui::rgba(0x676770ee);
    let colour_dotted = gpui::rgba(0x67677088);
    for seg in arrows.iter() {
        let col = match seg.style {
            ArrowStyle::Solid => colour_solid,
            ArrowStyle::Dotted => colour_dotted,
        };
        let x = lane_x(seg.lane);
        // Vertical segment for this row.
        let (v_top, v_height) = match seg.role {
            ArrowRole::Pass => (0., row_h),
            ArrowRole::Source => match seg.direction {
                ArrowDirection::Down => (mid, row_h - mid),
                ArrowDirection::Up => (0., mid),
            },
            ArrowRole::Target => match seg.direction {
                ArrowDirection::Down => (0., mid),
                ArrowDirection::Up => (mid, row_h - mid),
            },
        };
        gutter = gutter.child(
            div()
                .absolute()
                .left(px(x))
                .top(px(v_top))
                .w(px(ARROW_THICKNESS))
                .h(px(v_height))
                .bg(col),
        );
        // Source / target rows also get a horizontal stub at the row
        // middle, running from the lane right to the gutter's right
        // edge. Targets stop short to leave room for an arrowhead.
        if matches!(seg.role, ArrowRole::Source | ArrowRole::Target) {
            let stub_end = match seg.role {
                ArrowRole::Target => LISTING_GUTTER_WIDTH - ARROW_HEAD_LEN,
                _ => LISTING_GUTTER_WIDTH,
            };
            gutter = gutter.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(mid - ARROW_THICKNESS / 2.))
                    .w(px(stub_end - x))
                    .h(px(ARROW_THICKNESS))
                    .bg(col),
            );
            // Filled right-pointing triangle for the target arrowhead.
            // gpui has no transforms, so the wedge is composed of
            // stacked 1 px tall horizontal bars all sharing the same
            // left edge (the base). Each bar's width shrinks linearly
            // with |dy| so the right edge forms the diagonal faces
            // converging at the tip. Tip lands at the gutter's right
            // edge to visually touch the address column.
            if matches!(seg.role, ArrowRole::Target) {
                let base_x = LISTING_GUTTER_WIDTH - ARROW_HEAD_LEN;
                let half = ARROW_HEAD_HALF as i32;
                for dy in -half..=half {
                    let abs_dy = dy.unsigned_abs() as f32;
                    let bar_w =
                        ARROW_HEAD_LEN * (1.0 - abs_dy / (half as f32));
                    if bar_w <= 0. {
                        continue;
                    }
                    let bar_top = mid + dy as f32 - 0.5;
                    gutter = gutter.child(
                        div()
                            .absolute()
                            .left(px(base_x))
                            .top(px(bar_top))
                            .w(px(bar_w))
                            .h(px(1.))
                            .bg(col),
                    );
                }
            }
        }
    }
    gutter
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
        ListingRow::BasicBlockSeparator { arrows } => h_shift(
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(render_arrow_gutter(arrows, BB_SEPARATOR_HEIGHT))
                .child(
                    div()
                        .flex_1()
                        .h(px(1.))
                        .bg(rgb(COLOUR_BB_SEPARATOR)),
                ),
            h_offset,
            BB_SEPARATOR_HEIGHT,
            row_index,
            ctx,
        ),
        ListingRow::Instruction {
            address,
            bytes,
            mnemonic,
            operands,
            comment,
            arrows,
        } => {
            let mut row_div = div()
                .flex()
                .flex_row()
                .items_center()
                .text_base()
                .font_family("Courier New")
                // CF arrow gutter — vertical/horizontal lane segments.
                .child(render_arrow_gutter(arrows, LISTING_ROW_HEIGHT))
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
            h_shift_with_addr(
                row_div,
                h_offset,
                LISTING_ROW_HEIGHT,
                row_index,
                ctx,
                Some(*address),
            )
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
    /// Control-flow graph for the function whose entry is `entry_addr`.
    Cfg {
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
    },
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
    /// CFG view: camera + lazily-built function CFG + UI state.
    /// `Some` only for tabs with `TabKind::Cfg`.
    cfg: Option<CfgViewState>,
}

/// Per-tab state for a CFG view. Holds the camera (pan + zoom in
/// world units), the lazily-computed `FunctionCfg` for the tab's
/// entry address, and bookkeeping for pan-drag interaction.
#[derive(Clone)]
struct CfgViewState {
    /// Pan in world units. `(0, 0)` puts the world origin at the
    /// viewport's centre.
    pan_x: f32,
    pan_y: f32,
    /// Zoom multiplier — 1.0 means one world unit = `CFG_WORLD_UNIT`
    /// pixels. Clamped on input to [CFG_MIN_ZOOM, CFG_MAX_ZOOM].
    zoom: f32,
    /// CFG data; built on first paint and reused thereafter.
    cfg: Option<Arc<glass_arch_arm64::FunctionCfg>>,
    /// Viewport bounds in window coordinates, captured by a canvas
    /// hook each frame so pan/zoom math can convert between mouse
    /// positions and world coordinates.
    viewport_bounds: gpui::Bounds<Pixels>,
    /// `Some(start)` while the user is mid-pan-drag, recording the
    /// mouse position where the drag started (in window coords) and
    /// the pan offset that was current at drag start.
    drag_start: Option<(gpui::Point<Pixels>, f32, f32)>,
}

impl CfgViewState {
    fn new(pan_x: f32, pan_y: f32, zoom: f32) -> Self {
        Self {
            pan_x,
            pan_y,
            zoom: zoom.clamp(CFG_MIN_ZOOM, CFG_MAX_ZOOM),
            cfg: None,
            viewport_bounds: gpui::Bounds::default(),
            drag_start: None,
        }
    }
}

/// Pixels per world unit at zoom = 1.0. World coords are normalised
/// against this so a block at `(x, y)` lands at screen pixel
/// `(viewport_centre + (x - pan_x) * world_unit * zoom)`.
const CFG_WORLD_UNIT: f32 = 180.;
const CFG_MIN_ZOOM: f32 = 0.05;
const CFG_MAX_ZOOM: f32 = 4.;
/// Zoom-step factor for a single notch of trackpad / wheel scroll.
const CFG_ZOOM_STEP: f32 = 1.1;

/// LOD threshold — measured in *pixels of a block's on-screen size*
/// (its width at the current zoom). Below `LOD_PILL_MAX`, a block is
/// just a coloured pill with its label; above it, the block shows
/// the symbol header + first instructions + count summary.
const LOD_PILL_MAX: f32 = 50.;

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
    Cfg {
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
    },
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
            TabKind::Cfg { artifact, entry_addr } => glass_db::TabState::Cfg {
                artifact: artifact.clone(),
                entry_addr: *entry_addr,
                // Camera is owned by the Tab's CfgViewState (set at
                // resolve time); 0/0/1 is the open-fresh default.
                pan_x: 0.,
                pan_y: 0.,
                zoom: 1.,
            },
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
            LeafKind::Cfg { artifact, entry_addr } => TabKind::Cfg {
                artifact: artifact.clone(),
                entry_addr: *entry_addr,
            },
        }
    }
}

impl Tab {
    fn new(kind: TabKind) -> Self {
        let cfg = matches!(kind, TabKind::Cfg { .. })
            .then(|| CfgViewState::new(0., 0., 1.));
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
            cfg,
        }
    }

    /// Constructor that seeds the camera from persisted state. Used
    /// by the restore path so reopening a CFG tab puts the viewport
    /// back where the user left it.
    fn new_cfg_with_camera(
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
        pan_x: f32,
        pan_y: f32,
        zoom: f32,
    ) -> Self {
        let mut tab = Self::new(TabKind::Cfg { artifact, entry_addr });
        tab.cfg = Some(CfgViewState::new(pan_x, pan_y, zoom));
        tab
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
    /// Right-click context menu state. `None` when no menu is open.
    context_menu: Option<ContextMenuState>,
}

/// Floating context menu summoned by right-click on a listing row.
/// Position is in window coordinates; the renderer offsets a panel by
/// these.
#[derive(Clone)]
struct ContextMenuState {
    position: gpui::Point<Pixels>,
    items: Vec<ContextMenuItem>,
}

#[derive(Clone, Debug)]
enum ContextMenuItem {
    /// Open the CFG view for the function whose entry is `entry_addr`.
    /// `label` is the demangled function name shown in the menu item.
    ShowCfg {
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
        label: SharedString,
    },
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
            context_menu: None,
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
            open_tabs: self
                .tabs
                .iter()
                .map(|t| match (&t.kind, t.cfg.as_ref()) {
                    (
                        TabKind::Cfg { artifact, entry_addr },
                        Some(view),
                    ) => glass_db::TabState::Cfg {
                        artifact: artifact.clone(),
                        entry_addr: *entry_addr,
                        pan_x: view.pan_x,
                        pan_y: view.pan_y,
                        zoom: view.zoom,
                    },
                    _ => t.kind.to_state(),
                })
                .collect(),
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
            // CFG tabs are persisted with their camera state; restore
            // both the kind and the camera in one step.
            if let glass_db::TabState::Cfg {
                artifact,
                entry_addr,
                pan_x,
                pan_y,
                zoom,
            } = state
            {
                self.tabs.push(Tab::new_cfg_with_camera(
                    artifact.clone(),
                    *entry_addr,
                    *pan_x,
                    *pan_y,
                    *zoom,
                ));
                continue;
            }
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
            TabKind::Cfg { artifact, entry_addr } => {
                let name = bundle
                    .symbol_maps
                    .get(artifact)
                    .and_then(|sm| sm.at(*entry_addr))
                    .map(|s| s.display_name.clone())
                    .unwrap_or_else(|| format!("sub_{entry_addr:x}"));
                SharedString::from(format!("CFG: {name}"))
            }
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
            // CFG: the data is built lazily on first paint inside
            // render_cfg (it has a borrow of the bundle there); no
            // up-front setup needed here.
            TabKind::Cfg { .. } => {}
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

    /// Right-click handler invoked from a Listing row. Looks up the
    /// covering function for `addr` and opens a context menu offering
    /// "Show CFG" for it. No-op when `addr` isn't inside any function.
    fn open_listing_context_menu(
        &mut self,
        artifact: glass_db::ArtifactId,
        addr: u64,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle() else { return };
        let Some(sm) = bundle.symbol_maps.get(&artifact) else { return };
        let Some(sym) = sm.covering(addr) else { return };
        let label = SharedString::from(sym.display_name.clone());
        let entry_addr = sym.address;
        self.context_menu = Some(ContextMenuState {
            position,
            items: vec![ContextMenuItem::ShowCfg {
                artifact,
                entry_addr,
                label,
            }],
        });
        cx.notify();
    }

    fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.is_some() {
            self.context_menu = None;
            cx.notify();
        }
    }

    fn activate_context_menu_item(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.context_menu.as_ref() else { return };
        let Some(item) = menu.items.get(index).cloned() else { return };
        self.context_menu = None;
        match item {
            ContextMenuItem::ShowCfg {
                artifact,
                entry_addr,
                label,
            } => {
                self.show_cfg(artifact, entry_addr, label, cx);
            }
        }
    }

    /// Open (or focus an existing) CFG tab for a function. The CFG
    /// data itself is built lazily on the first paint, so opening a
    /// huge function is cheap up-front.
    fn show_cfg(
        &mut self,
        artifact: glass_db::ArtifactId,
        entry_addr: u64,
        _label: SharedString,
        cx: &mut Context<Self>,
    ) {
        let kind = TabKind::Cfg { artifact, entry_addr };
        if let Some(i) = self.tabs.iter().position(|t| t.kind == kind) {
            self.active_tab = Some(i);
        } else {
            self.tabs.push(Tab::new(kind));
            self.active_tab = Some(self.tabs.len() - 1);
        }
        self.overflow_open = false;
        cx.notify();
        self.save_state();
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

    /// Find the text section containing `addr` and open / focus a
    /// Listing tab scrolled to it. Used by CFG-block clicks where we
    /// only know the address, not the section name.
    fn open_listing_at_addr(
        &mut self,
        artifact: glass_db::ArtifactId,
        addr: u64,
        cx: &mut Context<Self>,
    ) {
        let section = match self
            .bundle()
            .and_then(|b| b.text_section_for_addr(&artifact, addr))
        {
            Some(s) => s.to_string(),
            None => return,
        };
        self.open_listing_at(artifact, section, addr, cx);
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

        let context_menu_overlay: Option<gpui::AnyElement> =
            self.context_menu.as_ref().map(|menu| {
                self.render_context_menu(menu, panel, border, fg, accent, cx)
                    .into_any_element()
            });

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
                this.close_context_menu(cx);
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
        if let Some(o) = context_menu_overlay {
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

    /// Render the right-click context menu as a small floating panel
    /// positioned at the click site. An occluded backdrop covers the
    /// window so clicks outside dismiss the menu without falling
    /// through to whatever's underneath.
    fn render_context_menu(
        &self,
        menu: &ContextMenuState,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        accent: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let weak = cx.entity().downgrade();

        let mut panel_el = div()
            .absolute()
            .left(menu.position.x)
            .top(menu.position.y)
            .min_w(px(220.))
            .bg(panel)
            .border_1()
            .border_color(border)
            .rounded_sm()
            .text_color(fg)
            .text_sm()
            .font_family("Menlo")
            .occlude()
            // Eat clicks inside the menu so the backdrop's
            // dismiss-on-click handler doesn't fire when the user
            // moves between items.
            .on_mouse_down(
                gpui::MouseButton::Left,
                |_ev, _w, cx: &mut App| {
                    cx.stop_propagation();
                },
            );

        for (index, item) in menu.items.iter().enumerate() {
            let (label_text, hint) = match item {
                ContextMenuItem::ShowCfg { label, .. } => {
                    (format!("Show CFG"), label.clone())
                }
            };
            let weak = weak.clone();
            panel_el = panel_el.child(
                div()
                    .id(("context-menu-item", index))
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .cursor_pointer()
                    .hover(|s| s.bg(accent))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_ev, _w, cx: &mut App| {
                            cx.stop_propagation();
                            if let Some(entity) = weak.upgrade() {
                                cx.update_entity(&entity, |shell, cx| {
                                    shell.activate_context_menu_item(index, cx);
                                });
                            }
                        },
                    )
                    .child(div().child(label_text))
                    .child(
                        div()
                            .flex_1()
                            .text_color(rgb(0x808088))
                            .child(hint),
                    ),
            );
        }

        let weak_for_backdrop = cx.entity().downgrade();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |_ev, _w, cx: &mut App| {
                    if let Some(entity) = weak_for_backdrop.upgrade() {
                        cx.update_entity(&entity, |shell, cx| {
                            shell.close_context_menu(cx);
                        });
                    }
                },
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                {
                    let weak = cx.entity().downgrade();
                    move |_ev, _w, cx: &mut App| {
                        if let Some(entity) = weak.upgrade() {
                            cx.update_entity(&entity, |shell, cx| {
                                shell.close_context_menu(cx);
                            });
                        }
                    }
                },
            )
            .child(panel_el)
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
            Some(TabKind::Cfg { artifact, entry_addr }) => self
                .render_cfg(&bundle, &artifact, entry_addr, panel, border, fg, dim, cx)
                .into_any_element(),
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
                        // Open at the section start, not at the per-pixel
                        // cursor address. The strip is too compressed for
                        // sub-section addressing to be meaningful — a
                        // 1 px hover on a 5 KiB section spans hundreds
                        // of bytes — and opening at the end of a section
                        // looks like the listing is "stuck at the end"
                        // of the disassembly.
                        let addr = sec.address;
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

    /// Render the CFG canvas for the function at `entry_addr` in
    /// `artifact`. The graph is built lazily on the first paint; the
    /// blocks are placed in world space (one rank per `BlockLayout.y`
    /// unit, columns at `BlockLayout.x`) and the camera maps them to
    /// screen pixels.
    fn render_cfg(
        &mut self,
        bundle: &LoadedBundle,
        artifact: &glass_db::ArtifactId,
        entry_addr: u64,
        panel: gpui::Rgba,
        border: gpui::Rgba,
        fg: gpui::Rgba,
        dim: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Ensure the CFG is built. We snapshot the Arc here so the
        // render closure can capture it without borrowing self.
        self.ensure_cfg_built(artifact, entry_addr);

        let Some(active_idx) = self.active_tab else {
            return div().size_full().bg(panel).into_any_element();
        };
        let cfg_view = self
            .tabs
            .get(active_idx)
            .and_then(|t| t.cfg.as_ref())
            .cloned();
        let Some(view) = cfg_view else {
            return div().size_full().bg(panel).into_any_element();
        };
        let cfg = match view.cfg.clone() {
            Some(c) => c,
            None => {
                return div()
                    .size_full()
                    .bg(panel)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(dim)
                    .child(SharedString::from(format!(
                        "No CFG for function at 0x{entry_addr:x}"
                    )))
                    .into_any_element();
            }
        };

        let zoom = view.zoom;
        let pan_x = view.pan_x;
        let pan_y = view.pan_y;

        // Look up function display name for the header.
        let func_name = bundle
            .symbol_maps
            .get(artifact)
            .and_then(|sm| sm.at(entry_addr))
            .map(|s| s.display_name.clone())
            .unwrap_or_else(|| format!("sub_{entry_addr:x}"));

        // World-to-screen converter. The world origin maps to the
        // viewport's centre; `pan_x`/`pan_y` shift the world relative
        // to that. One world unit = `CFG_WORLD_UNIT * zoom` pixels.
        let bounds = view.viewport_bounds;
        let bounds_origin_x = bounds.origin.x.as_f32();
        let bounds_origin_y = bounds.origin.y.as_f32();
        let bounds_width = bounds.size.width.as_f32();
        let bounds_height = bounds.size.height.as_f32();
        let centre_x = bounds_origin_x + bounds_width / 2.;
        let centre_y = bounds_origin_y + bounds_height / 2.;
        let unit = CFG_WORLD_UNIT * zoom;
        // First-paint guard: the canvas measure hook fires during
        // this same paint, so the *current* viewport_bounds is
        // still its default (0×0) on frame 1. Disable culling in
        // that case so all blocks render — they may overflow off
        // the canvas, but the next paint (triggered by the canvas
        // hook's notify) has the real bounds and re-culls.
        let bounds_unknown = bounds_width <= 0. || bounds_height <= 0.;

        let weak = cx.entity().downgrade();

        // ---- Sizing model ------------------------------------------
        //
        // Each block's *content* is at most: optional symbol header,
        // an address row, up to 3 instructions (mnemonic + operands),
        // and either an ellipsis row or an instruction-count row.
        //
        // We size every block to fit its content snugly: width = the
        // longest line × an approximate char width, clamped between
        // MIN_W and MAX_W. Height = exactly the number of content
        // rows × a per-row world height.
        // Physical text metrics. Rounded up from gpui's rendered
        // sizes plus a couple of px of slack on each row so subpixel
        // rounding can't clip the last instruction. Under-estimating
        // here costs us a visible row; over-estimating just adds a
        // little dead space at the bottom.
        const ROW_PX: f32 = 17.;
        const ELLIPSIS_ROW_PX: f32 = 28.;
        const PADDING_PX_H: f32 = 18.;
        /// Safety margin shaved off the pixel budget before
        /// `plan_layout` accepts a layout. Belt-and-braces against
        /// subpixel rounding turning a "just fits" plan into a
        /// "clipped by 1 px" render.
        const HEIGHT_FUDGE_PX: f32 = 4.;
        // Pixel-space width metrics so we can pick a tight world
        // width per block. Courier at text_xs averages ~7 px/char.
        // PADDING_PX_W covers px_2 left/right + 2 px border with a
        // little breathing room on either side.
        const CHAR_PX: f32 = 7.;
        const PADDING_PX_W: f32 = 28.;
        const MIN_BLOCK_PX_W: f32 = 80.;
        const MAX_BLOCK_PX_W: f32 = 640.;
        // A "full" (truncated) block reserves this many *world*
        // units vertically. Translates to `FULL_BLOCK_WORLD_H × unit`
        // screen pixels, so as the user zooms in the block grows on
        // screen and more rows of text fit inside it. Zoom out and
        // the row budget shrinks down to a single line of "…
        // N instructions" + last.
        const FULL_BLOCK_WORLD_H: f32 = 0.6;
        const RANK_GAP: f32 = 0.6;
        const COL_GAP: f32 = 0.25;

        // ---- Row budget driven by pixel height -------------------
        //
        // A "full" (truncated) block has FULL_BLOCK_PX_H pixels of
        // screen height. The truncated layout renders:
        //   - optional symbol header        (ROW_PX)
        //   - `preview` instruction rows    (preview * ROW_PX)
        //   - one "… N instructions" line   (ELLIPSIS_ROW_PX, taller)
        //   - one last-instruction row      (ROW_PX)
        //   - top + bottom padding          (PADDING_PX_H)
        // and the total must fit in FULL_BLOCK_PX_H. Solving for
        // `preview` gives the budget below.
        // Per-frame screen budget for a truncated block, derived
        // from the constant *world* height: scales with zoom so
        // zooming in really does grow the block (and lets more
        // rows fit). At very small zoom the budget can drop below
        // even one row, in which case we sacrifice the ellipsis
        // line first and the last instruction last — the user must
        // always see at least one line, and preferably the last
        // instruction of the block.
        let full_block_px_h = FULL_BLOCK_WORLD_H * unit;
        // Effective budget used by `plan_layout` to decide whether
        // a candidate layout fits. The fudge ensures the rendered
        // height never ends up over the actual block height after
        // subpixel rounding.
        let budget_px_h = full_block_px_h - HEIGHT_FUDGE_PX;

        // Plan the block layout given the pixel budget. Picks the
        // most informative layout that fits.
        let plan_layout = move |b: &glass_arch_arm64::BasicBlock,
                                 has_symbol: bool|
         -> CfgLayoutPlan {
            let n = b.instructions.len();
            if n == 0 {
                return CfgLayoutPlan {
                    preview: 0,
                    show_ellipsis: false,
                    show_last: false,
                };
            }
            let sym_h = if has_symbol { ROW_PX } else { 0. };
            // The full-show layout: sym? + n × ROW_PX + padding.
            let full_h = sym_h + (n as f32) * ROW_PX + PADDING_PX_H;
            if full_h <= budget_px_h {
                return CfgLayoutPlan {
                    preview: n,
                    show_ellipsis: false,
                    show_last: false,
                };
            }
            // Try the truncated layout: maximize preview while
            // keeping (sym? + preview + ellipsis + last + padding)
            // within the budget.
            let mut best_preview: Option<usize> = None;
            for k in 0..n.saturating_sub(1) {
                let h = sym_h
                    + (k as f32) * ROW_PX
                    + ELLIPSIS_ROW_PX
                    + ROW_PX
                    + PADDING_PX_H;
                if h <= budget_px_h {
                    best_preview = Some(k);
                } else {
                    break;
                }
            }
            if let Some(preview) = best_preview {
                return CfgLayoutPlan {
                    preview,
                    show_ellipsis: true,
                    show_last: true,
                };
            }
            // Budget too small for ellipsis+last. Try ellipsis+last
            // only (no preview rows):
            //   sym? + ellipsis + last + padding
            if sym_h + ELLIPSIS_ROW_PX + ROW_PX + PADDING_PX_H <= budget_px_h {
                return CfgLayoutPlan {
                    preview: 0,
                    show_ellipsis: true,
                    show_last: true,
                };
            }
            // Tighter still: just the last instruction.
            if sym_h + ROW_PX + PADDING_PX_H <= budget_px_h {
                return CfgLayoutPlan {
                    preview: 0,
                    show_ellipsis: false,
                    show_last: true,
                };
            }
            // Smallest fit: just `… N instructions` with no
            // last-instruction row. The user knows the block has
            // content but it's about to collapse to the pill LOD.
            CfgLayoutPlan {
                preview: 0,
                show_ellipsis: true,
                show_last: false,
            }
        };

        let symbols = bundle.symbol_maps.get(artifact);
        let symbol_for_block = |b: &glass_arch_arm64::BasicBlock| -> Option<SharedString> {
            symbols
                .and_then(|sm| sm.at(b.start_addr))
                .map(|s| SharedString::from(s.display_name.clone()))
        };
        // Resolve every call's target address to a function entry +
        // display name via the artifact's symbol map. Direct calls
        // (`bl <imm>`) get a resolved name; indirect calls (`blr`)
        // have target_addr = None and are skipped.
        let resolve_call =
            |addr: u64| -> Option<(u64, SharedString)> {
                let sym = symbols.and_then(|sm| sm.covering(addr))?;
                Some((sym.address, SharedString::from(sym.display_name.clone())))
            };
        let summaries: Vec<CfgBlockSummary> = cfg
            .blocks
            .iter()
            .map(|b| {
                let mut calls = std::collections::HashMap::new();
                for c in &b.calls {
                    if let Some(tgt) = c.target_addr {
                        if let Some(resolved) = resolve_call(tgt) {
                            calls.insert(c.site_addr, resolved);
                        }
                    }
                }
                CfgBlockSummary {
                    symbol: symbol_for_block(b),
                    calls,
                }
            })
            .collect();

        // Per-block size (world units). Width is sized from the
        // longest displayed line in screen pixels, then converted to
        // world units via `unit` so it stays visually constant
        // across zoom — at higher zoom the block doesn't waste
        // space on wider boxes. Height is exact (no dead space) and
        // accounts for the ellipsis row's larger height when
        // truncating.
        let block_size = |block: &glass_arch_arm64::BasicBlock,
                          summary: &CfgBlockSummary,
                          plan: CfgLayoutPlan|
         -> (f32, f32) {
            const ADDR_COL: usize = 16 + 1; // "0123456789abcdef "
            let mut longest = 0usize;
            if let Some(name) = summary.symbol.as_ref() {
                longest = longest.max(name.len() + 1); // ":" suffix
            }
            let insn_line_len = |insn: &glass_arch_arm64::InstructionEntry| -> usize {
                // When the operand is a call whose target resolved
                // to a symbol, we render the symbol name in place of
                // the raw operand text — size for that length so
                // long callee names don't get truncated.
                let operand_len = match summary.calls.get(&insn.address) {
                    Some((_, name)) => name.len(),
                    None => insn.operands.len(),
                };
                ADDR_COL
                    + insn.mnemonic.len()
                    + if operand_len == 0 { 0 } else { 1 + operand_len }
            };
            let has_sym = summary.symbol.is_some();
            let n = block.instructions.len();

            for insn in block.instructions.iter().take(plan.preview) {
                longest = longest.max(insn_line_len(insn));
            }
            if plan.show_ellipsis {
                let skipped = n
                    .saturating_sub(plan.preview)
                    .saturating_sub(if plan.show_last { 1 } else { 0 });
                let footer_len = 2 + format!("{skipped} instructions").len();
                longest = longest.max(footer_len);
            }
            if plan.show_last {
                if let Some(last) = block.instructions.last() {
                    longest = longest.max(insn_line_len(last));
                }
            }
            if n == 0 {
                longest = longest.max("(empty)".len());
            }
            // Width: longest-line pixels → world units.
            let w_px = ((longest as f32) * CHAR_PX + PADDING_PX_W)
                .clamp(MIN_BLOCK_PX_W, MAX_BLOCK_PX_W);
            let w = w_px / unit;

            // Height: sum of exactly what we'll render. Each row is
            // ROW_PX except the ellipsis (ELLIPSIS_ROW_PX).
            let mut content_px = PADDING_PX_H;
            if has_sym {
                content_px += ROW_PX;
            }
            content_px += (plan.preview as f32) * ROW_PX;
            if plan.show_ellipsis {
                content_px += ELLIPSIS_ROW_PX;
            }
            if plan.show_last {
                content_px += ROW_PX;
            }
            if n == 0 {
                content_px += ROW_PX; // (empty)
            }
            let h = content_px.max(ROW_PX + PADDING_PX_H) / unit;
            (w, h)
        };

        // Plan + size each block once. The plan is reused at render
        // time so layout sizing and rendering stay in lockstep.
        let plans: Vec<CfgLayoutPlan> = cfg
            .blocks
            .iter()
            .zip(summaries.iter())
            .map(|(b, s)| plan_layout(b, s.symbol.is_some()))
            .collect();
        let mut sizes: Vec<(f32, f32)> = Vec::with_capacity(cfg.blocks.len());
        for ((block, summary), plan) in
            cfg.blocks.iter().zip(summaries.iter()).zip(plans.iter())
        {
            sizes.push(block_size(block, summary, *plan));
        }

        // Group block indices by rank, preserving discovery order.
        let mut by_rank: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, layout) in cfg.layout.iter().enumerate() {
            by_rank.entry(layout.rank).or_default().push(i);
        }

        // Place each block using the CFG's barycenter-tuned x as a
        // hint. Within a rank we sort by hinted x, scale the hints
        // so they're proportional to block widths, then enforce
        // non-overlap (left-to-right) with COL_GAP between borders.
        // Centre each rank's pack on x = 0.
        let mut world_pos: Vec<(f32, f32)> = vec![(0., 0.); cfg.blocks.len()];
        let mut cursor_y = 0.0_f32;
        for (_rank, indices) in &by_rank {
            // Sort the rank by the layout's hinted x so the order
            // reflects the barycenter pass (relative parent/child
            // alignment), not just discovery order.
            let mut ordered: Vec<usize> = indices.clone();
            ordered.sort_by(|&a, &b| {
                cfg.layout[a]
                    .x
                    .partial_cmp(&cfg.layout[b].x)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(&b))
            });
            // Scale hinted x's to honour block widths: each block's
            // left edge starts at hint_x scaled to "average block
            // width + COL_GAP" so blocks roughly align with their
            // ideal positions but don't overlap.
            let max_h = ordered
                .iter()
                .map(|&i| sizes[i].1)
                .fold(0.0_f32, f32::max);
            // First pass: pick a left x for each block from the
            // hinted positions, scaled.
            let scale = ordered
                .iter()
                .map(|&i| sizes[i].0)
                .fold(0.0_f32, f32::max)
                .max(1.0)
                + COL_GAP;
            // Map hint_x ranges to actual placement: place each
            // block centred on hint_x * scale, then enforce min-gap.
            let mut placement: Vec<(f32, f32, usize)> = ordered
                .iter()
                .map(|&i| {
                    let w = sizes[i].0;
                    let hint = cfg.layout[i].x;
                    let left = hint * scale - w / 2.;
                    (left, w, i)
                })
                .collect();
            // Walk left-to-right: each block's left must be at
            // least previous.left + previous.w + COL_GAP.
            for k in 1..placement.len() {
                let (prev_left, prev_w, _) = placement[k - 1];
                let min_left = prev_left + prev_w + COL_GAP;
                if placement[k].0 < min_left {
                    placement[k].0 = min_left;
                }
            }
            // Centre the rank.
            let (first_left, _, _) = placement[0];
            let (last_left, last_w, _) = placement[placement.len() - 1];
            let total_extent = last_left + last_w - first_left;
            let shift = -first_left - total_extent / 2.;
            for &(left, _, i) in &placement {
                world_pos[i] = (left + shift, cursor_y);
            }
            cursor_y += max_h + RANK_GAP;
        }

        // Per-block screen rect, computed once, reused for blocks +
        // edges. Indexed by block id.
        struct ScreenRect {
            // In *local* (scene) pixel coordinates.
            x: f32,
            y: f32,
            w: f32,
            h: f32,
        }
        let mut rects: Vec<ScreenRect> = Vec::with_capacity(cfg.blocks.len());
        for (i, _block) in cfg.blocks.iter().enumerate() {
            let (world_x, world_y) = world_pos[i];
            let (w_world, h_world) = sizes[i];
            let screen_x_px = centre_x + (world_x - pan_x) * unit;
            let screen_y_px = centre_y + (world_y - pan_y) * unit;
            let screen_w_px = w_world * unit;
            let screen_h_px = h_world * unit;
            rects.push(ScreenRect {
                x: screen_x_px - bounds_origin_x,
                y: screen_y_px - bounds_origin_y,
                w: screen_w_px,
                h: screen_h_px,
            });
        }

        // Build the absolute-positioned scene.
        let mut scene = div()
            .id("cfg-scene")
            .absolute()
            .top_0()
            .left_0()
            .size_full();

        // ---- Edge routing prep -------------------------------------
        //
        // Pre-compute everything the router needs in screen-pixel
        // space. The router is built around two ideas:
        //
        // 1. *Rank-gap bands.* Between consecutive ranks lies an
        //    empty horizontal band (the RANK_GAP we inserted at
        //    placement). Horizontal edge segments live inside those
        //    bands so they never cross a block.
        //
        // 2. *Free vertical lanes.* A vertical x is "clear" across
        //    a range of ranks if no block in those ranks covers
        //    that x. For an edge from rank `R_s` to rank `R_t`, we
        //    walk candidate x's outward from the source/target
        //    columns until we find one clear of every block in
        //    ranks (R_s+1 .. R_t-1) and approach-side blocks in
        //    R_s/R_t. That's where the long vertical leg goes.
        let bounds_w = bounds_width;
        let bounds_h = bounds_height;
        let _ = (bounds_w, bounds_h);

        // Per-block fan-in/fan-out counts + edge ordering. We sort
        // each block's outgoing edges by the *target x* (so edges
        // exit the source in the same left-to-right order their
        // targets sit on screen) and incoming edges by *source x*.
        // This eliminates pointless crossings where edges with
        // targets on the right currently exit from a left-side slot.
        let mut in_edges: Vec<Vec<usize>> = vec![Vec::new(); cfg.blocks.len()];
        let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); cfg.blocks.len()];
        for (ei, edge) in cfg.edges.iter().enumerate() {
            if edge.to.0 < in_edges.len() {
                in_edges[edge.to.0].push(ei);
            }
            if edge.from.0 < out_edges.len() {
                out_edges[edge.from.0].push(ei);
            }
        }
        // For each block, sort outgoing by target x; incoming by
        // source x. Build per-edge slot index lookups.
        let mut out_slot: Vec<usize> = vec![0; cfg.edges.len()];
        let mut in_slot: Vec<usize> = vec![0; cfg.edges.len()];
        for (bi, eids) in out_edges.iter_mut().enumerate() {
            eids.sort_by(|&a, &b| {
                let xa = rects
                    .get(cfg.edges[a].to.0)
                    .map(|r| r.x + r.w / 2.)
                    .unwrap_or(0.);
                let xb = rects
                    .get(cfg.edges[b].to.0)
                    .map(|r| r.x + r.w / 2.)
                    .unwrap_or(0.);
                xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal)
            });
            for (slot, &ei) in eids.iter().enumerate() {
                out_slot[ei] = slot;
            }
            let _ = bi;
        }
        for (bi, eids) in in_edges.iter_mut().enumerate() {
            eids.sort_by(|&a, &b| {
                let xa = rects
                    .get(cfg.edges[a].from.0)
                    .map(|r| r.x + r.w / 2.)
                    .unwrap_or(0.);
                let xb = rects
                    .get(cfg.edges[b].from.0)
                    .map(|r| r.x + r.w / 2.)
                    .unwrap_or(0.);
                xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal)
            });
            for (slot, &ei) in eids.iter().enumerate() {
                in_slot[ei] = slot;
            }
            let _ = bi;
        }
        let in_total: Vec<usize> = in_edges.iter().map(|v| v.len()).collect();
        let out_total: Vec<usize> = out_edges.iter().map(|v| v.len()).collect();

        // For each rank: the y at the bottom of its tallest block,
        // the y at the top of the next rank below, and the list of
        // (x_left, x_right) intervals occupied by blocks (sorted).
        struct RankGeom {
            bottom_y: f32,
            next_top_y: f32,
            intervals: Vec<(f32, f32)>,
        }
        let rank_of_block: Vec<usize> = cfg.layout.iter().map(|l| l.rank).collect();
        let mut rank_geom: std::collections::BTreeMap<usize, RankGeom> =
            std::collections::BTreeMap::new();
        for (rank, indices) in &by_rank {
            let bottom_y = indices
                .iter()
                .map(|&i| rects[i].y + rects[i].h)
                .fold(f32::MIN, f32::max);
            let mut intervals: Vec<(f32, f32)> = indices
                .iter()
                .map(|&i| (rects[i].x, rects[i].x + rects[i].w))
                .collect();
            intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            rank_geom.insert(
                *rank,
                RankGeom {
                    bottom_y,
                    next_top_y: bottom_y, // filled in below
                    intervals,
                },
            );
        }
        let ranks_sorted: Vec<usize> = rank_geom.keys().copied().collect();
        for w in ranks_sorted.windows(2) {
            let upper = w[0];
            let lower = w[1];
            let next_top = by_rank[&lower]
                .iter()
                .map(|&i| rects[i].y)
                .fold(f32::MAX, f32::min);
            if let Some(entry) = rank_geom.get_mut(&upper) {
                entry.next_top_y = next_top;
            }
        }
        // Bounds of the canvas content — used as fallback "outside"
        // lanes when no internal channel is free.
        let scene_left = rects
            .iter()
            .map(|r| r.x)
            .fold(f32::MAX, f32::min)
            - 24.;
        let scene_right = rects
            .iter()
            .map(|r| r.x + r.w)
            .fold(f32::MIN, f32::max)
            + 24.;

        // Per-rank-gap horizontal-lane allocator. Each entry maps
        // `rank` → list of (allocated_y) for edges using the gap
        // below it. We assign each edge its own y within the gap.
        let mut h_lanes: std::collections::BTreeMap<usize, Vec<f32>> =
            std::collections::BTreeMap::new();
        // Per-vertical-lane allocator. Vertical lanes are picked by
        // x; multiple edges in the same x channel get stacked
        // horizontally with a small offset so they don't overlap.
        let mut v_lane_count: std::collections::HashMap<i32, usize> =
            std::collections::HashMap::new();
        // Returns a clear vertical x between source and target,
        // searching outward from `prefer` (typically the average of
        // source and target x). Skips any x that crosses a block in
        // ranks strictly between the source and target.
        fn pick_vertical_lane(
            prefer: f32,
            rank_lo: usize,
            rank_hi: usize,
            rank_geom: &std::collections::BTreeMap<usize, RankGeom>,
            scene_left: f32,
            scene_right: f32,
        ) -> f32 {
            // A vertical at x is clear across ranks [lo+1, hi-1]
            // (the intermediate ranks the edge crosses) when no
            // block in any of those ranks contains x. The source
            // and target ranks themselves are exited / entered via
            // the rank-gap turns, so we don't need to clear them.
            let blocks: &Vec<(f32, f32)> = &{
                let mut out: Vec<(f32, f32)> = Vec::new();
                for r in (rank_lo.min(rank_hi))..=(rank_lo.max(rank_hi)) {
                    if r == rank_lo || r == rank_hi {
                        continue;
                    }
                    if let Some(g) = rank_geom.get(&r) {
                        out.extend(g.intervals.iter().copied());
                    }
                }
                out
            };
            let clear = |x: f32| -> bool {
                let margin = 4.;
                !blocks
                    .iter()
                    .any(|&(l, r)| x >= l - margin && x <= r + margin)
            };
            if clear(prefer) {
                return prefer;
            }
            // Walk outward in expanding steps until we find a free x
            // or hit the canvas bounds.
            let step = 12.;
            for k in 1..200 {
                let dx = step * k as f32;
                let left = prefer - dx;
                if left >= scene_left && clear(left) {
                    return left;
                }
                let right = prefer + dx;
                if right <= scene_right && clear(right) {
                    return right;
                }
                if left < scene_left && right > scene_right {
                    break;
                }
            }
            // Nothing found — fall back to the side highway.
            if (prefer - scene_left).abs() < (prefer - scene_right).abs() {
                scene_left
            } else {
                scene_right
            }
        }

        // ---- Edges first so blocks render on top of them. ----------
        for (edge_idx, edge) in cfg.edges.iter().enumerate() {
            let Some(src) = rects.get(edge.from.0) else { continue };
            let Some(dst) = rects.get(edge.to.0) else { continue };
            let from_idx = edge.from.0;
            let to_idx = edge.to.0;

            // Fan-in / fan-out attach fractions, ordered by the
            // x position of the *other* end. So edges to the right
            // exit through the right portion of the source's bottom
            // edge; edges from the left enter through the left
            // portion of the target's top edge.
            let out_n = out_total[from_idx].max(1);
            let in_n = in_total[to_idx].max(1);
            let out_frac =
                (out_slot[edge_idx] + 1) as f32 / (out_n + 1) as f32;
            let in_frac =
                (in_slot[edge_idx] + 1) as f32 / (in_n + 1) as f32;
            let sx = src.x + src.w * out_frac;
            let sy = src.y + src.h;
            let tx = dst.x + dst.w * in_frac;
            let ty = dst.y;

            let both_off = !bounds_unknown
                && ((sx < 0. && tx < 0.)
                    || (sx > bounds_w && tx > bounds_w)
                    || (sy < 0. && ty < 0.)
                    || (sy > bounds_h && ty > bounds_h));
            if both_off {
                continue;
            }
            let dotted = matches!(
                edge.kind,
                glass_arch_arm64::BlockEdgeKind::TakenConditional
                    | glass_arch_arm64::BlockEdgeKind::NotTakenConditional,
            );

            let from_rank = rank_of_block.get(from_idx).copied().unwrap_or(0);
            let to_rank = rank_of_block.get(to_idx).copied().unwrap_or(0);

            // Horizontal lane y for the source's rank gap. Each
            // edge in the same gap stacks vertically by 4 px.
            let gap_top = rank_geom
                .get(&from_rank)
                .map(|g| g.bottom_y)
                .unwrap_or(sy);
            let gap_bottom = rank_geom
                .get(&from_rank)
                .map(|g| g.next_top_y)
                .unwrap_or(sy + 24.);
            let gap_mid = (gap_top + gap_bottom) / 2.;
            let lanes = h_lanes.entry(from_rank).or_default();
            let lane_idx = lanes.len();
            // Distribute lanes around the gap midline.
            let lane_step = 5.;
            let lane_y = gap_mid + ((lane_idx as f32 / 2.).ceil() as f32)
                * lane_step
                * if lane_idx % 2 == 0 { 1. } else { -1. };
            // Clamp inside the gap.
            let half = ((gap_bottom - gap_top).abs() / 2. - 4.).max(0.);
            let lane_y = lane_y.clamp(gap_mid - half, gap_mid + half);
            lanes.push(lane_y);

            // Routing modes:
            //   - Forward adjacent rank: 3-segment route via rank-gap.
            //   - Forward multi-rank: 5-segment route via a clear
            //     vertical channel.
            //   - Back-edge (to_rank <= from_rank): exit source side,
            //     run up the side highway, enter target side.
            let single_rank_forward = to_rank == from_rank + 1;
            let is_back_edge = to_rank <= from_rank;
            let segments: Vec<EdgeSegment>;
            let arrow_pos: (f32, f32, ArrowHeadDir);

            // Pixels the final line segment is shortened by so the
            // arrowhead's wedge body isn't painted over by the line.
            const ARROW_TRIM_PX: f32 = 7.;

            if single_rank_forward {
                // Simple 3-segment route via the rank-gap lane.
                let final_y_top = lane_y.min(ty);
                let final_y_len = (ty - lane_y).abs() - ARROW_TRIM_PX;
                segments = vec![
                    EdgeSegment {
                        x: sx,
                        y: sy.min(lane_y),
                        length: (lane_y - sy).abs(),
                        horizontal: false,
                    },
                    EdgeSegment {
                        x: sx.min(tx),
                        y: lane_y,
                        length: (tx - sx).abs(),
                        horizontal: true,
                    },
                    EdgeSegment {
                        x: tx,
                        y: final_y_top,
                        length: final_y_len.max(0.),
                        horizontal: false,
                    },
                ];
                arrow_pos = (tx, ty, ArrowHeadDir::Down);
            } else if is_back_edge {
                // Back-edge: route via a vertical highway clear of
                // every block, entering the target's side. Pick the
                // highway side that gives the cleanest path — the
                // side furthest from the source/target column range
                // so we never cross either block.
                let exit_y = src.y + src.h * out_frac;
                let entry_y = dst.y + dst.h * in_frac;
                // Try both sides; pick whichever yields a clear
                // vertical lane closer to the source.
                let right_prefer = src.x.max(dst.x + dst.w) + 24.;
                let left_prefer = src.x.min(dst.x) - 24.;
                let right_lane = pick_vertical_lane(
                    right_prefer,
                    from_rank,
                    to_rank,
                    &rank_geom,
                    scene_left,
                    scene_right,
                );
                let left_lane = pick_vertical_lane(
                    left_prefer,
                    from_rank,
                    to_rank,
                    &rank_geom,
                    scene_left,
                    scene_right,
                );
                // Choose whichever side gives a shorter total
                // horizontal travel.
                let right_cost = (right_lane - (src.x + src.w)).abs()
                    + (right_lane - (dst.x + dst.w)).abs();
                let left_cost = (left_lane - src.x).abs()
                    + (left_lane - dst.x).abs();
                let use_right = right_cost <= left_cost;
                let v_lane_x = if use_right { right_lane } else { left_lane };
                // Exit and entry sides face the highway.
                let exit_side_x = if use_right { src.x + src.w } else { src.x };
                let entry_side_x = if use_right { dst.x + dst.w } else { dst.x };
                let key = (v_lane_x / 6.).round() as i32;
                let n = v_lane_count.entry(key).or_insert(0);
                let v_offset = (*n as f32)
                    * 4.
                    * if use_right { 1. } else { -1. };
                *n += 1;
                let v_x = v_lane_x + v_offset;

                // Trim the final horizontal segment so the
                // arrowhead's wedge body isn't covered by the line.
                let (h3_x, h3_len) = if use_right {
                    // Line comes from the right, ends at the target's
                    // right side. Stop ARROW_TRIM_PX away.
                    let stop_x = entry_side_x + ARROW_TRIM_PX;
                    (
                        stop_x.min(v_x),
                        ((v_x - stop_x).abs() - 0.).max(0.),
                    )
                } else {
                    // Line comes from the left, ends at the target's
                    // left side. Stop ARROW_TRIM_PX away.
                    let stop_x = entry_side_x - ARROW_TRIM_PX;
                    (
                        v_x.min(stop_x),
                        ((stop_x - v_x).abs() - 0.).max(0.),
                    )
                };
                segments = vec![
                    // 1: horizontal from source side to highway.
                    EdgeSegment {
                        x: exit_side_x.min(v_x),
                        y: exit_y,
                        length: (v_x - exit_side_x).abs(),
                        horizontal: true,
                    },
                    // 2: vertical at v_x from exit_y to entry_y.
                    EdgeSegment {
                        x: v_x,
                        y: exit_y.min(entry_y),
                        length: (entry_y - exit_y).abs(),
                        horizontal: false,
                    },
                    // 3: horizontal from highway to target side
                    //    (stops short of the arrowhead).
                    EdgeSegment {
                        x: h3_x,
                        y: entry_y,
                        length: h3_len,
                        horizontal: true,
                    },
                ];
                // Arrow enters target's side. If we exited and
                // entered on the right, the line approaches the
                // target from its right and the arrow points Left.
                // If on the left, the arrow points Right.
                arrow_pos = (
                    entry_side_x,
                    entry_y,
                    if use_right {
                        ArrowHeadDir::Left
                    } else {
                        ArrowHeadDir::Right
                    },
                );
            } else {
                // Forward multi-rank. Pick a vertical lane outside
                // any intermediate block. Bias toward the average of
                // source and target x so edges don't all pile on
                // the same side.
                let prefer = (sx + tx) / 2.;
                let v_lane_x = pick_vertical_lane(
                    prefer,
                    from_rank,
                    to_rank,
                    &rank_geom,
                    scene_left,
                    scene_right,
                );
                let key = (v_lane_x / 6.).round() as i32;
                let n = v_lane_count.entry(key).or_insert(0);
                let v_offset = (*n as f32) * 4.;
                *n += 1;
                let v_x = v_lane_x + v_offset;

                let target_gap_top = rank_geom
                    .iter()
                    .filter(|(r, _)| **r + 1 == to_rank)
                    .map(|(_, g)| g.bottom_y)
                    .next()
                    .unwrap_or(ty - 24.);
                let target_gap_bottom = rank_geom
                    .iter()
                    .filter(|(r, _)| **r + 1 == to_rank)
                    .map(|(_, g)| g.next_top_y)
                    .next()
                    .unwrap_or(ty);
                let approach_y = (target_gap_top + target_gap_bottom) / 2.;

                let final_y_top = approach_y.min(ty);
                let final_y_len = (ty - approach_y).abs() - ARROW_TRIM_PX;
                segments = vec![
                    EdgeSegment {
                        x: sx,
                        y: sy.min(lane_y),
                        length: (lane_y - sy).abs(),
                        horizontal: false,
                    },
                    EdgeSegment {
                        x: sx.min(v_x),
                        y: lane_y,
                        length: (v_x - sx).abs(),
                        horizontal: true,
                    },
                    EdgeSegment {
                        x: v_x,
                        y: lane_y.min(approach_y),
                        length: (approach_y - lane_y).abs(),
                        horizontal: false,
                    },
                    EdgeSegment {
                        x: v_x.min(tx),
                        y: approach_y,
                        length: (tx - v_x).abs(),
                        horizontal: true,
                    },
                    EdgeSegment {
                        x: tx,
                        y: final_y_top,
                        length: final_y_len.max(0.),
                        horizontal: false,
                    },
                ];
                arrow_pos = (tx, ty, ArrowHeadDir::Down);
            }
            for seg in segments {
                scene = scene.child(render_edge_segment(seg, dotted));
            }
            scene = scene.child(render_edge_arrowhead(
                arrow_pos.0,
                arrow_pos.1,
                arrow_pos.2,
            ));
        }

        // ---- Blocks ------------------------------------------------
        for (i, block) in cfg.blocks.iter().enumerate() {
            let rect = &rects[i];
            // Cull off-viewport blocks. Skip culling on the first
            // paint when the viewport bounds aren't known yet —
            // otherwise only the block at the origin would render
            // and the user would have to pan to trigger a refresh.
            let off_screen = !bounds_unknown
                && (rect.x + rect.w < 0.
                    || rect.x > bounds_width
                    || rect.y + rect.h < 0.
                    || rect.y > bounds_height);
            if off_screen {
                continue;
            }
            let summary = &summaries[i];
            // LOD selection based on on-screen width.
            let block_el = if rect.w < LOD_PILL_MAX {
                render_cfg_block_pill(block, summary, dim)
            } else {
                let block_ctx = CfgBlockRenderCtx {
                    shell: weak.clone(),
                    artifact: artifact.clone(),
                    block_idx: i,
                };
                render_cfg_block_content(block, summary, plans[i], Some(&block_ctx))
            };
            let click_weak = weak.clone();
            let click_artifact = artifact.clone();
            let block_addr = block.start_addr;
            scene = scene.child(
                div()
                    .id(("cfg-block", i))
                    .absolute()
                    .left(px(rect.x))
                    .top(px(rect.y))
                    .w(px(rect.w))
                    .h(px(rect.h))
                    .cursor_pointer()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        move |_ev, _w, cx: &mut App| {
                            if let Some(entity) = click_weak.upgrade() {
                                let artifact = click_artifact.clone();
                                cx.update_entity(&entity, |shell, cx| {
                                    shell.open_listing_at_addr(
                                        artifact, block_addr, cx,
                                    );
                                });
                            }
                        },
                    )
                    .child(block_el),
            );
        }

        // Capture viewport bounds each frame so pan/zoom math has
        // current values.
        let bounds_weak = weak.clone();
        let measure = gpui::canvas(
            move |bounds, _window, cx| {
                if let Some(entity) = bounds_weak.upgrade() {
                    cx.update_entity(&entity, |shell, _cx| {
                        if let Some(idx) = shell.active_tab {
                            if let Some(tab) = shell.tabs.get_mut(idx) {
                                if let Some(view) = tab.cfg.as_mut() {
                                    view.viewport_bounds = bounds;
                                }
                            }
                        }
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let header = div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .border_b_1()
            .border_color(border)
            .text_sm()
            .text_color(fg)
            .font_family("Menlo")
            .child(SharedString::from(func_name))
            .child(
                div()
                    .text_color(dim)
                    .child(SharedString::from(format!(
                        "{} blocks · {} edges · zoom {:.0}%",
                        cfg.blocks.len(),
                        cfg.edges.len(),
                        zoom * 100.,
                    ))),
            );

        // Event handlers on the canvas surface.
        let zoom_weak = weak.clone();
        let pan_weak = weak.clone();
        let drag_weak = weak.clone();
        let drag_move_weak = weak.clone();
        let drag_end_weak = weak.clone();

        let canvas_body = div()
            .id("cfg-canvas")
            .flex_1()
            .relative()
            .overflow_hidden()
            .bg(panel)
            .child(measure)
            .child(scene)
            // Trackpad / mouse-wheel: cmd or ctrl held = zoom around
            // cursor; otherwise pan.
            .on_scroll_wheel(move |ev: &gpui::ScrollWheelEvent, _w, cx| {
                if let Some(entity) = zoom_weak.upgrade() {
                    cx.update_entity(&entity, |shell, cx| {
                        let delta = ev.delta.pixel_delta(px(20.));
                        // Zoom on Shift / Cmd / Ctrl + scroll. Plain
                        // scroll pans (trackpad gesture or wheel).
                        if ev.modifiers.shift
                            || ev.modifiers.platform
                            || ev.modifiers.control
                        {
                            // Shift+wheel turns vertical scroll into
                            // horizontal on some mice, so fall back
                            // to whichever axis carries the input.
                            let raw = if delta.y.as_f32().abs() > 0. {
                                delta.y.as_f32()
                            } else {
                                delta.x.as_f32()
                            };
                            shell.cfg_zoom_by(ev.position, raw, cx);
                        } else {
                            shell.cfg_pan_by(delta.x.as_f32(), delta.y.as_f32(), cx);
                        }
                    });
                }
                let _ = pan_weak;
            })
            // Mouse drag pan (middle button or left+space; for v1 we
            // accept any-button drag).
            .on_mouse_down(
                gpui::MouseButton::Middle,
                move |ev: &gpui::MouseDownEvent, _w, cx| {
                    if let Some(entity) = drag_weak.upgrade() {
                        let pos = ev.position;
                        cx.update_entity(&entity, |shell, _cx| {
                            shell.cfg_drag_start(pos);
                        });
                    }
                },
            )
            .on_mouse_move(move |ev: &gpui::MouseMoveEvent, _w, cx| {
                if let Some(entity) = drag_move_weak.upgrade() {
                    let pos = ev.position;
                    cx.update_entity(&entity, |shell, cx| {
                        shell.cfg_drag_move(pos, cx);
                    });
                }
            })
            .on_mouse_up(
                gpui::MouseButton::Middle,
                move |_ev: &gpui::MouseUpEvent, _w, cx| {
                    if let Some(entity) = drag_end_weak.upgrade() {
                        cx.update_entity(&entity, |shell, _cx| {
                            shell.cfg_drag_end();
                        });
                    }
                },
            );

        div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(panel)
            .child(header)
            .child(canvas_body)
            .into_any_element()
    }

    fn ensure_cfg_built(&mut self, artifact: &glass_db::ArtifactId, entry_addr: u64) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        if view.cfg.is_some() {
            return;
        }
        let bundle = match &self.state {
            ShellState::Ready(b) => b,
            _ => return,
        };
        let Some(symbols) = bundle.symbol_maps.get(artifact) else { return };
        let cfg = build_cfg_from_text_sections(
            &bundle.text_sections,
            symbols,
            artifact,
            entry_addr,
        );
        view.cfg = cfg.map(Arc::new);
    }

    fn cfg_pan_by(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        let unit = CFG_WORLD_UNIT * view.zoom;
        if unit <= 0. {
            return;
        }
        view.pan_x -= dx / unit;
        view.pan_y -= dy / unit;
        cx.notify();
    }

    fn cfg_zoom_by(
        &mut self,
        anchor: gpui::Point<Pixels>,
        delta: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        let old_zoom = view.zoom;
        // Pinch / cmd-scroll: positive delta -> zoom in.
        let factor = if delta > 0. {
            CFG_ZOOM_STEP
        } else if delta < 0. {
            1. / CFG_ZOOM_STEP
        } else {
            return;
        };
        let new_zoom = (old_zoom * factor).clamp(CFG_MIN_ZOOM, CFG_MAX_ZOOM);
        if (new_zoom - old_zoom).abs() < f32::EPSILON {
            return;
        }

        // Zoom around the cursor so the point under the mouse stays
        // anchored. Convert anchor (window coords) to world coords at
        // the old zoom, then re-solve pan so that world point lands
        // back at the same screen position at the new zoom.
        let bounds = view.viewport_bounds;
        let centre_x = bounds.origin.x.as_f32() + bounds.size.width.as_f32() / 2.;
        let centre_y = bounds.origin.y.as_f32() + bounds.size.height.as_f32() / 2.;
        let old_unit = CFG_WORLD_UNIT * old_zoom;
        let new_unit = CFG_WORLD_UNIT * new_zoom;
        let anchor_x = anchor.x.as_f32();
        let anchor_y = anchor.y.as_f32();
        let world_x = view.pan_x + (anchor_x - centre_x) / old_unit;
        let world_y = view.pan_y + (anchor_y - centre_y) / old_unit;
        view.zoom = new_zoom;
        view.pan_x = world_x - (anchor_x - centre_x) / new_unit;
        view.pan_y = world_y - (anchor_y - centre_y) / new_unit;
        cx.notify();
    }

    fn cfg_drag_start(&mut self, pos: gpui::Point<Pixels>) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        view.drag_start = Some((pos, view.pan_x, view.pan_y));
    }

    fn cfg_drag_move(&mut self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        let Some((start_pos, start_pan_x, start_pan_y)) = view.drag_start else {
            return;
        };
        let unit = CFG_WORLD_UNIT * view.zoom;
        if unit <= 0. {
            return;
        }
        view.pan_x = start_pan_x - (pos.x.as_f32() - start_pos.x.as_f32()) / unit;
        view.pan_y = start_pan_y - (pos.y.as_f32() - start_pos.y.as_f32()) / unit;
        cx.notify();
    }

    fn cfg_drag_end(&mut self) {
        let Some(idx) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        let Some(view) = tab.cfg.as_mut() else { return };
        view.drag_start = None;
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
