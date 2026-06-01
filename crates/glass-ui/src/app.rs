//! Window + menu plumbing.
//!
//! `launch` is the only `pub` here: it owns the gpui application and
//! wires actions + menus + the first window. The rest are private
//! helpers for opening windows, restoring window bounds, spawning the
//! background loader, and the periodic glass-db flush.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use gpui::{
    prelude::*, px, size, App, Bounds, Context, KeyBinding, QuitMode, Window, WindowBounds,
    WindowOptions,
};
use gpui_platform::application;

use crate::loader::load_bundle_blocking;
use crate::{
    About, CloseFile, CloseWindow, Copy, NewWindow, OpenFile, OpenRecent0, OpenRecent1, OpenRecent2, OpenRecent3,
    OpenRecent4, OpenRecent5, OpenRecent6, OpenRecent7, OpenRecent8, OpenRecent9,
    PaletteActivate, PaletteClose, PaletteDown, PaletteUp, Progress, Quit, Shell, ShellState,
    Theme0, Theme1, Theme2, Theme3, Theme4, Theme5, Theme6, Theme7,
    HexCursorLeft,
    HexCursorRight,
    ListingPageDown,
    ListingPageUp,
    PaletteModeBinary,
    PaletteModeText,
    ToggleChangesDialog,
    TogglePalette,
};

const RECENT_SLOTS: usize = 10;

/// On macOS, the leftmost menu-bar item's title comes from the
/// process name, *not* from `cx.set_menus`.
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
    // Install a panic hook that the live smali op editor can
    // tell to stay quiet on a thread-local basis. Without this,
    // every keystroke against a partial op line dumps a panic
    // backtrace from the smali parser to stderr — caught
    // correctly by `parse_smali_class`, but extremely noisy.
    glass_api::install_suppressible_panic_hook();
    let db = match glass_db::Database::open(fresh) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!("could not open glass-db: {e:#}; persistence disabled");
            None
        }
    };
    application().with_assets(crate::IconAssets).run(move |cx: &mut App| {
        cx.init_colors();
        // Quit when the last window closes — the default on macOS
        // keeps the process alive in the dock, which doesn't match
        // how a command-line-launched tool is expected to behave.
        cx.set_quit_mode(QuitMode::LastWindowClosed);
        cx.bind_keys([
            KeyBinding::new("cmd-f", TogglePalette, None),
            KeyBinding::new("escape", PaletteClose, None),
            KeyBinding::new("up", PaletteUp, None),
            KeyBinding::new("down", PaletteDown, None),
            KeyBinding::new("enter", PaletteActivate, None),
            KeyBinding::new("cmd-1", PaletteModeText, None),
            KeyBinding::new("cmd-2", PaletteModeBinary, None),
            KeyBinding::new("pageup", ListingPageUp, None),
            KeyBinding::new("pagedown", ListingPageDown, None),
            KeyBinding::new("left", HexCursorLeft, None),
            KeyBinding::new("right", HexCursorRight, None),
            KeyBinding::new("cmd-e", ToggleChangesDialog, None),
            KeyBinding::new("cmd-o", OpenFile, None),
            KeyBinding::new("cmd-n", NewWindow, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-shift-w", CloseFile, None),
            KeyBinding::new("cmd-c", Copy, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);

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
        // Close File → tell the focused Shell window to drop its
        // bundle and return to the launched-empty state. Deferred via
        // cx.spawn for the same reason as About: the active window's
        // slot is currently taken while this action callback runs.
        // Copy → tell the focused Shell window to write its
        // "selected thing" to the clipboard. Skipped automatically
        // when a TextInput has focus (the input's own cmd-c handler
        // wins in gpui's action-dispatch ordering).
        cx.on_action(|_: &Copy, cx: &mut App| {
            cx.spawn(async move |cx| {
                cx.update(|cx| {
                    let Some(wh) = cx.active_window() else { return };
                    let Some(typed) = wh.downcast::<Shell>() else { return };
                    let _ = cx.update_window(typed.into(), |root, _w, cx| {
                        if let Ok(entity) = root.downcast::<Shell>() {
                            entity.update(cx, |shell, cx| {
                                shell.copy_current_to_clipboard(cx);
                            });
                        }
                    });
                });
            })
            .detach();
        });
        cx.on_action(|_: &CloseFile, cx: &mut App| {
            cx.spawn(async move |cx| {
                cx.update(|cx| {
                    let Some(wh) = cx.active_window() else { return };
                    let Some(typed) = wh.downcast::<Shell>() else { return };
                    let _ = cx.update_window(typed.into(), |root, _w, cx| {
                        if let Ok(entity) = root.downcast::<Shell>() {
                            entity.update(cx, |shell, cx| shell.close_file(cx));
                        }
                    });
                });
            })
            .detach();
        });
        // About → tell every Shell window to show its About modal.
        // Deferred via cx.spawn so we never read a window that's on
        // gpui's window stack during this menu callback.
        cx.on_action(|_: &About, cx: &mut App| {
            cx.spawn(async move |cx| {
                cx.update(|cx| {
                    for wh in cx.windows() {
                        if let Some(typed) = wh.downcast::<Shell>() {
                            let _ = cx.update_window(typed.into(), |root, _w, cx| {
                                if let Ok(entity) = root.downcast::<Shell>() {
                                    entity.update(cx, |shell, cx| shell.open_about(cx));
                                }
                            });
                        }
                    }
                });
            })
            .detach();
        });

        register_open_recent_actions(db.clone(), cx);
        register_theme_actions(cx, db.clone());
        set_app_menus(cx, db.as_ref());

        open_glass_window(path.clone(), db.clone(), cx);
        cx.activate(true);
    });
    Ok(())
}

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

fn register_theme_actions(cx: &mut App, db: Option<glass_db::Database>) {
    macro_rules! wire {
        ($action:ty, $idx:expr) => {{
            let db = db.clone();
            cx.on_action(move |_: &$action, cx: &mut App| {
                apply_nth_theme($idx, cx);
                // Rebuild menus so the "● " marker moves to the
                // newly active item.
                set_app_menus(cx, db.as_ref());
            });
        }};
    }
    wire!(Theme0, 0);
    wire!(Theme1, 1);
    wire!(Theme2, 2);
    wire!(Theme3, 3);
    wire!(Theme4, 4);
    wire!(Theme5, 5);
    wire!(Theme6, 6);
    wire!(Theme7, 7);
}

/// Apply theme #`idx` from the current `ThemeSet` to every Shell
/// window, persisting the choice to `WindowSettings.theme` so the
/// next launch starts on it. No-ops if the index is out of range.
fn apply_nth_theme(idx: usize, cx: &mut App) {
    let set = crate::theme::ThemeSet::load();
    let Some(theme) = set.all().get(idx).cloned() else { return };
    let name = theme.name.clone();
    // Persist first so even if no Shell window exists, the next launch
    // picks up the new choice.
    let mut settings = glass_db::load_window_settings();
    settings.theme = Some(name.clone());
    let _ = glass_db::save_window_settings(&settings);
    // Push the new theme into every live window. Deferred via
    // cx.spawn for the same reason `About` is — we may be inside a
    // menu callback and reading the active window directly would
    // hit gpui's "window is on the stack" assertion.
    cx.spawn(async move |cx| {
        cx.update(|cx| {
            for wh in cx.windows() {
                if let Some(typed) = wh.downcast::<Shell>() {
                    let name = name.clone();
                    let _ = cx.update_window(typed.into(), |root, _w, cx| {
                        if let Ok(entity) = root.downcast::<Shell>() {
                            entity.update(cx, |shell, cx| shell.set_theme(&name, cx));
                        }
                    });
                }
            }
        });
    })
    .detach();
}

fn open_nth_recent(db: Option<glass_db::Database>, idx: usize, cx: &mut App) {
    let Some(handle) = db.clone() else { return };
    let recents = handle.recent_bundles(RECENT_SLOTS);
    let Some(rec) = recents.into_iter().nth(idx) else { return };
    let Some(path) = rec.source_path else { return };
    open_path(PathBuf::from(path), db, cx);
}

/// Open `path` — reuse the first empty Glass window if one is open,
/// otherwise spawn a fresh window. Empty = the user hasn't loaded
/// anything into this window yet (no bundle), so dropping a load
/// into it doesn't lose any state.
///
/// Defers the decision via `cx.spawn` so it runs after the current
/// menu / action callback unwinds. While a menu callback is on the
/// stack, the active window's slot is taken, and any path that
/// inspects it (read_window via Entity::read, etc.) hits a
/// `.expect("attempted to read a window that is already on the stack")`
/// inside gpui. Spawning sidesteps the problem entirely.
fn open_path(path: PathBuf, db: Option<glass_db::Database>, cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.update(|cx| open_path_now(path, db, cx));
    })
    .detach();
}

fn open_path_now(path: PathBuf, db: Option<glass_db::Database>, cx: &mut App) {
    let empty_shell: Option<gpui::Entity<Shell>> =
        cx.windows().into_iter().find_map(|wh| {
            let typed = wh.downcast::<Shell>()?;
            // update_window returns Err for any window currently on
            // the stack (the slot has been .take()n out). Treat that
            // as "not reusable" — we'd rather open a fresh window
            // than abort.
            cx.update_window(typed.into(), |root_view, _w, cx| {
                let entity = root_view.downcast::<Shell>().ok()?;
                if entity.read(cx).is_empty() {
                    Some(entity)
                } else {
                    None
                }
            })
            .ok()
            .flatten()
        });
    if let Some(shell) = empty_shell {
        shell.update(cx, |s, _| {
            s.source_path = Some(path.clone());
            s.state = ShellState::Loading;
        });
        spawn_loader(&shell, path, cx);
    } else {
        open_glass_window(Some(path), db, cx);
    }
}

/// Up to this many themes appear in the View → Theme submenu. The
/// `actions!` list above has a matching set of Theme0..ThemeN slots.
const THEME_SLOTS: usize = 8;

fn build_theme_items() -> Vec<gpui::MenuItem> {
    let set = crate::theme::ThemeSet::load();
    let themes = set.all();
    let active = glass_db::load_window_settings().theme;
    if themes.is_empty() {
        return vec![gpui::MenuItem::action("No themes installed", Theme0).disabled(true)];
    }
    themes
        .iter()
        .take(THEME_SLOTS)
        .enumerate()
        .map(|(i, t)| {
            // Mark the active theme with a leading bullet. macOS
            // doesn't expose a native "checked" item via the menu
            // API we use, so a glyph prefix is the simplest signal.
            let is_active = active.as_deref() == Some(&t.name);
            let label = if is_active {
                format!("● {}", t.name)
            } else {
                format!("   {}", t.name)
            };
            match i {
                0 => gpui::MenuItem::action(label, Theme0),
                1 => gpui::MenuItem::action(label, Theme1),
                2 => gpui::MenuItem::action(label, Theme2),
                3 => gpui::MenuItem::action(label, Theme3),
                4 => gpui::MenuItem::action(label, Theme4),
                5 => gpui::MenuItem::action(label, Theme5),
                6 => gpui::MenuItem::action(label, Theme6),
                _ => gpui::MenuItem::action(label, Theme7),
            }
        })
        .collect()
}

fn set_app_menus(cx: &mut App, db: Option<&glass_db::Database>) {
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
            gpui::MenuItem::action("About Glass", About),
            gpui::MenuItem::separator(),
            gpui::MenuItem::action("Quit", Quit),
        ]),
        gpui::Menu::new("File").items({
            let mut items: Vec<gpui::MenuItem> = vec![
                gpui::MenuItem::action("Open…", OpenFile),
                gpui::MenuItem::submenu(
                    gpui::Menu::new("Open Recent").items(recent_items),
                ),
                gpui::MenuItem::action("Close File", CloseFile),
                gpui::MenuItem::separator(),
                gpui::MenuItem::action("New Window", NewWindow),
                gpui::MenuItem::action("Close Window", CloseWindow),
            ];
            items.shrink_to_fit();
            items
        }),
        gpui::Menu::new("View").items([
            gpui::MenuItem::action("Search…", TogglePalette),
            gpui::MenuItem::separator(),
            gpui::MenuItem::submenu(
                gpui::Menu::new("Theme").items(build_theme_items()),
            ),
        ]),
        gpui::Menu::new("Window").items([]),
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
        cx.update(|cx| open_path(path, db, cx));
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
    let mut bounds = match settings.bounds {
        Some(b) => Bounds {
            origin: gpui::point(px(b.x), px(b.y)),
            size: size(px(b.width), px(b.height)),
        },
        None => Bounds::centered(None, size(px(1200.), px(800.)), cx),
    };
    // Stagger windows so a 2nd / 3rd window doesn't open exactly on
    // top of the existing one(s). Step is small enough that the
    // window stays visible on screen but big enough that the title
    // bar of the underneath window peeks out.
    const STAGGER_PX: f32 = 32.;
    let stagger = (cx.windows().len() as f32) * STAGGER_PX;
    if stagger > 0. {
        bounds.origin.x += px(stagger);
        bounds.origin.y += px(stagger);
    }
    let path_for_window = path.clone();
    let db_for_window = db.clone();
    cx.open_window(
        WindowOptions {
            focus: true,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| {
            // Set an explicit OS-level window title. Without this,
            // AppKit picks up the executable name ("glass" — lower-
            // case) and that string then shows up in the Window menu
            // and elsewhere. Loader updates this to the bundle name
            // once a bundle finishes loading.
            window.set_window_title("Glass");
            let shell = cx.new(|cx| {
                Shell::new(path_for_window.clone(), db_for_window.clone(), window, cx)
            });
            // Populate the Frida scripts panel from the user's
            // library before the first render — the panel
            // refresh API expects a `Context<Shell>`, so we run
            // it through update_entity here rather than inside
            // `Shell::new`.
            shell.update(cx, |shell, cx| {
                shell.refresh_scripts(cx);
            });
            if let Some(p) = path_for_window.clone() {
                spawn_loader(&shell, p, cx);
            }
            if let Some(db) = db_for_window.clone() {
                spawn_flush_timer(&shell, db, cx);
            }
            spawn_annotation_reload_poll(&shell, cx);
            spawn_device_poll(&shell, cx);
            spawn_frida_probe(&shell, cx);
            spawn_debug_dock_pump(&shell, cx);
            shell.update(cx, |_shell, cx: &mut Context<Shell>| {
                cx.observe_window_bounds(window, |_shell, window: &mut Window, _cx| {
                    // `window.bounds()` is the full frame (including
                    // title bar). gpui's `open_window` interprets the
                    // `WindowBounds::Windowed(bounds)` we pass back as
                    // a *content* rect — AppKit then adds the title
                    // bar around it. If we save the frame and restore
                    // it as content, the window grows by one title
                    // bar each launch. Convert to content here so the
                    // round-trip is stable.
                    let frame = window.bounds();
                    let content = window.viewport_size();
                    let titlebar_h = (frame.size.height - content.height).max(px(0.));
                    let mut settings = glass_db::load_window_settings();
                    settings.bounds = Some(glass_db::StoredBounds {
                        x: frame.origin.x.as_f32(),
                        y: (frame.origin.y + titlebar_h).as_f32(),
                        width: content.width.as_f32(),
                        height: content.height.as_f32(),
                    });
                    let _ = glass_db::save_window_settings(&settings);
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
/// Watch the glass-db file's mtime; when it changes (typically
/// because a CLI / MCP invocation wrote an annotation), reload
/// annotations into the in-memory index for every artifact in
/// the current bundle. 2-second cadence — cheap (one stat call)
/// and well below the latency users notice between running an
/// external write and switching back to the GUI.
fn spawn_annotation_reload_poll(shell: &gpui::Entity<Shell>, cx: &mut App) {
    let Ok(db_path) = glass_db::default_db_path() else {
        return;
    };
    let weak = shell.downgrade();
    cx.spawn(async move |cx| {
        let mut last_mtime: Option<std::time::SystemTime> =
            std::fs::metadata(&db_path).ok().and_then(|m| m.modified().ok());
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;
            let Some(entity) = weak.upgrade() else { break };
            let Some(current) = std::fs::metadata(&db_path)
                .ok()
                .and_then(|m| m.modified().ok())
            else {
                continue;
            };
            if Some(current) != last_mtime {
                last_mtime = Some(current);
                cx.update_entity(&entity, |shell, cx| {
                    shell.refresh_all_annotations(cx);
                });
            }
        }
    })
    .detach();
}

/// Periodic device-discovery snapshot. Runs `DeviceManager::
/// list()` every 2.5s and pushes the result through
/// `cx.update_entity` so the toolbar's device chip stays in
/// sync with what's plugged in. Cheap — adb is a local socket
/// call, usbmuxd is similar.
fn spawn_device_poll(shell: &gpui::Entity<Shell>, cx: &mut App) {
    let weak = shell.downgrade();
    // Snapshot the manager handle once outside the loop —
    // `Arc::clone` is cheap and avoids re-entering the entity
    // every tick just to grab it.
    let manager = cx.update_entity(shell, |s, _cx| s.device_manager.clone());
    cx.spawn(async move |cx| {
        // Probe backend status once at startup. Manager
        // re-uses cached info on subsequent calls.
        if let Some(entity) = weak.upgrade() {
            let status = manager.backend_status();
            cx.update_entity(&entity, |shell, cx| {
                shell.device_backend_status = status;
                cx.notify();
            });
        }
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(2500))
                .await;
            let Some(entity) = weak.upgrade() else { break };
            // Run the list call off the foreground so a slow
            // adb invocation can't stall the UI thread.
            let manager = manager.clone();
            let snapshot = cx
                .background_executor()
                .spawn(async move { manager.list() })
                .await;
            cx.update_entity(&entity, |shell, cx| {
                // If the selected device disappeared, drop it
                // back to None so the chip flips to "No device"
                // instead of pretending we're still attached.
                if let Some(id) = shell.selected_device.clone() {
                    if !snapshot.iter().any(|d| d.id == id) {
                        shell.selected_device = None;
                    }
                }
                shell.device_snapshot = snapshot;
                cx.notify();
            });
        }
    })
    .detach();
}

/// Probe the currently-selected device for frida-server. Runs
/// off the foreground (frida-core's calls block, even the
/// cheap-looking ones). Cache TTL is 10s — frida-server
/// doesn't come and go, but we want a freshly-started server
/// to show up in the chip within a reasonable interval.
///
/// Only probes when `Shell.selected_device` is `Some`. Result
/// goes into `Shell.frida_probes`; the chip reads from there
/// at render time.
fn spawn_frida_probe(shell: &gpui::Entity<Shell>, cx: &mut App) {
    let weak = shell.downgrade();
    cx.spawn(async move |cx| {
        // 3 seconds is short enough that uninstall / install /
        // app-launch state changes show up in the chip within
        // a few ticks, and long enough that the chip doesn't
        // do the frida round-trip on every render. The probe
        // itself is cheap but it does block a tokio worker.
        const PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(2000))
                .await;
            let Some(entity) = weak.upgrade() else { break };
            // Decide whether the selected device needs a probe.
            // Pull the answer out of the Shell synchronously.
            let target = cx.update_entity(&entity, |shell, _cx| {
                let id = shell.selected_device.clone()?;
                let stale = shell
                    .frida_probes
                    .get(&id)
                    .map(|c| c.probed_at.elapsed() >= PROBE_TTL)
                    .unwrap_or(true);
                if stale { Some(id) } else { None }
            });
            let Some(id) = target else { continue };
            // Snapshot the bits the cross-check needs:
            //   * Package name from the loaded bundle's manifest
            //     (`android:package`). Used to verify a
            //     gadget candidate is *actually* the app the
            //     user has loaded, not a stale frida-core
            //     cache entry from a previous session.
            //   * Device manager handle for the ADB
            //     fallback verification.
            let package_name = cx
                .update_entity(&entity, |shell, _cx| {
                    shell
                        .bundle()
                        .and_then(|b| b.android_manifest.as_ref())
                        .and_then(|m| m.package_name().map(|s| s.to_string()))
                });
            let device_manager = cx.update_entity(&entity, |shell, _cx| {
                shell.device_manager.clone()
            });
            // Run the actual frida call on a dedicated OS
            // thread, with a hard timeout on the async side.
            // frida-core's `enumerate_processes` can block
            // indefinitely against a device that's connected
            // but unresponsive (no frida-server running, ADB
            // shim in a weird state, etc.). Without the
            // timeout the probe future never completes, so
            // `frida_probes` never gets a result and the chip
            // sticks on "probing Frida…". We synthesise a
            // ServerUnreachable result on timeout so the UI
            // can fall through to the gadget-port check and,
            // failing that, surface the inject option.
            let probe_id = id.clone();
            let (tx, rx) = std::sync::mpsc::sync_channel::<
                Result<glass_frida::ProbeReport, glass_frida::FridaError>,
            >(1);
            std::thread::spawn(move || {
                let _ = tx.send(glass_frida::FridaRuntime::probe(&probe_id));
            });
            const PROBE_TIMEOUT: std::time::Duration =
                std::time::Duration::from_secs(4);
            // Poll the channel + race against a timer task.
            // 50 ms granularity is plenty for a 4 s budget and
            // keeps the poll task itself responsive.
            let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
            let result = loop {
                match rx.try_recv() {
                    Ok(r) => break r,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        break Err(glass_frida::FridaError::ServerUnreachable);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        device = %id.serial,
                        "frida probe timed out after {PROBE_TIMEOUT:?}; \
                         treating as unreachable",
                    );
                    break Err(glass_frida::FridaError::ServerUnreachable);
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
            };
            // Cross-check gadget reports against ADB. Frida-
            // core caches process listings aggressively and
            // continues to enumerate gadgeted apps after
            // they've been killed or uninstalled. We trust
            // it when it says Server (frida-server is a
            // long-lived daemon; the cache rarely lies); for
            // Gadget we need an independent "yes the app is
            // running" signal from ADB.
            let final_result = reconcile_gadget_via_adb(
                &id,
                result,
                package_name.as_deref(),
                &device_manager,
            );
            cx.update_entity(&entity, |shell, cx| {
                shell.frida_probes.insert(
                    id,
                    crate::FridaProbeCache {
                        result: final_result,
                        probed_at: std::time::Instant::now(),
                    },
                );
                cx.notify();
            });
        }
    })
    .detach();
}

/// Reconcile the frida-core probe against an independent
/// gadget-port probe via ADB.
///
/// Why the two-step:
///   * frida-core's `enumerate_processes` lists the device's
///     whole `ps` table, so it can only reliably surface
///     `frida-server` (a daemon with a known name). Anything
///     else in that list isn't a useful "gadget is alive"
///     signal — gadget mode is per-app and frida-core caches
///     state.
///   * The gadget binds TCP port 27042 inside the app's
///     sandbox *only* when its host app is running with the
///     gadget loaded. ADB-forwarding to that port + a TCP
///     connect+read is a direct, cache-free reachability
///     check.
///
/// Behaviour:
///   * Frida says Server → trust it.
///   * Frida says anything else (incl. ServerUnreachable) →
///     run the gadget-port probe. If it answers, synthesise
///     a `Gadget` report. Otherwise pass the frida result
///     through unchanged.
fn reconcile_gadget_via_adb(
    device: &glass_device::DeviceId,
    result: Result<glass_frida::ProbeReport, glass_frida::FridaError>,
    _expected_package: Option<&str>,
    device_manager: &std::sync::Arc<glass_device::DeviceManager>,
) -> Result<glass_frida::ProbeReport, glass_frida::FridaError> {
    // Server reports we trust as-is.
    if let Ok(r) = &result {
        if matches!(r.kind, glass_frida::FridaKind::Server) {
            return result;
        }
    }
    // ADB only makes sense for Android.
    if !matches!(device.platform, glass_device::DevicePlatform::Android) {
        return result;
    }
    let Ok(status) = device_manager.backend_status().adb.clone() else {
        return result;
    };
    let backend = match glass_device::adb::AdbBackend::with_override(
        Some(status.binary_path),
    ) {
        Ok(b) => b,
        Err(_) => return result,
    };
    match backend.probe_gadget(&device.serial) {
        Ok(true) => {
            tracing::info!("gadget probe: 27042 alive — reporting Gadget");
            Ok(glass_frida::ProbeReport {
                kind: glass_frida::FridaKind::Gadget,
                // Frida-core wouldn't talk to a gadget we
                // hadn't asked it about; we don't have a
                // useful version string here. Empty is fine
                // — the chip falls back to "frida-gadget"
                // without a version suffix.
                agent_version: None,
                os: Some("android".to_string()),
                gadget_process_names: Vec::new(),
            })
        }
        Ok(false) => {
            tracing::info!("gadget probe: 27042 closed");
            result
        }
        Err(e) => {
            tracing::info!(?e, "gadget probe: ADB error");
            result
        }
    }
}

/// Drains the active Frida session's event channel each tick
/// and appends formatted lines to the dock log. Runs as long
/// as the dock has a `Session`. Cheap when nothing's ready —
/// `poll_events` returns immediately on an empty channel.
fn spawn_debug_dock_pump(shell: &gpui::Entity<Shell>, cx: &mut App) {
    let weak = shell.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            let Some(entity) = weak.upgrade() else { break };
            // Pull a clone of the session (cheap — just two
            // Arcs) so we can drain events without holding a
            // borrow on Shell.
            let session_opt: Option<glass_frida::Session> =
                cx.update_entity(&entity, |shell, _cx| {
                    shell
                        .debug_dock
                        .as_ref()
                        .and_then(|d| d.session.clone())
                });
            let Some(session) = session_opt else {
                continue;
            };
            let events = session.poll_events();
            if events.is_empty() {
                continue;
            }
            cx.update_entity(&entity, |shell, cx| {
                for ev in events {
                    route_session_event(shell, ev);
                }
                cx.notify();
            });
        }
    })
    .detach();
}

/// Dispatch a SessionEvent to either the trace registry (if
/// the script belongs to a live trace) or the dock's general
/// log (for the smoke-test path and one-off scripts).
fn route_session_event(shell: &mut Shell, ev: glass_frida::SessionEvent) {
    match ev {
        glass_frida::SessionEvent::ScriptMessage { script_id, payload } => {
            // Look up the script first as a trace; failing
            // that, as a hook. Hook + trace can't share an
            // id (they get separate IDs from the session
            // actor) so the ordering here is purely
            // cosmetic.
            let trace_key = shell
                .bundle()
                .and_then(|b| b.traces.key_for_script(script_id).cloned());
            if let Some(key) = trace_key {
                let inv = invocation_from_payload(&payload);
                let class_short = key
                    .class_jni
                    .strip_prefix('L')
                    .and_then(|s| s.strip_suffix(';'))
                    .map(|s| s.replace('/', "."))
                    .unwrap_or_else(|| key.class_jni.clone());
                let line = format!(
                    "{class_short}.{}  {}",
                    key.method_name, inv.summary
                );
                if let Some(bundle) = shell.bundle_mut() {
                    bundle.traces.push_invocation(&key, inv);
                }
                push_dock_log_line(shell, line);
                return;
            }
            let hook_key = shell
                .bundle()
                .and_then(|b| b.hooks.key_for_script(script_id).cloned());
            if let Some(key) = hook_key {
                let inv = invocation_from_payload(&payload);
                let class_short = key
                    .class_jni
                    .strip_prefix('L')
                    .and_then(|s| s.strip_suffix(';'))
                    .map(|s| s.replace('/', "."))
                    .unwrap_or_else(|| key.class_jni.clone());
                // Hooks render with an extra prefix marker so
                // the user can distinguish trace vs hook
                // events in the unified log.
                let line = format!(
                    "⚙ {class_short}.{}  {}",
                    key.method_name, inv.summary
                );
                // Build hook invocation reusing the trace
                // helper since the wire format is identical.
                let hook_inv = crate::hooks::Invocation {
                    at: inv.at,
                    kind: inv.kind,
                    summary: inv.summary,
                };
                if let Some(bundle) = shell.bundle_mut() {
                    bundle.hooks.push_invocation(&key, hook_inv);
                }
                push_dock_log_line(shell, line);
                return;
            }
            // Untracked script (smoke test or M3.4 plumbing).
            push_dock_log_line(
                shell,
                format!("[script {script_id}] {payload}"),
            );
        }
        glass_frida::SessionEvent::ScriptLog {
            script_id,
            level,
            message,
        } => push_dock_log_line(
            shell,
            format!("[script {script_id} {level}] {message}"),
        ),
        glass_frida::SessionEvent::ScriptError {
            script_id,
            description,
        } => {
            // Mark the matching trace OR hook as failed —
            // a script ID is unique to one registry, so at
            // most one of these branches fires.
            let trace_key = shell
                .bundle()
                .and_then(|b| b.traces.key_for_script(script_id).cloned());
            if let Some(key) = trace_key {
                if let Some(bundle) = shell.bundle_mut() {
                    bundle.traces.mark_failed(&key, description.clone());
                }
            } else {
                let hook_key = shell
                    .bundle()
                    .and_then(|b| b.hooks.key_for_script(script_id).cloned());
                if let Some(key) = hook_key {
                    if let Some(bundle) = shell.bundle_mut() {
                        bundle.hooks.mark_failed(&key, description.clone());
                    }
                }
            }
            push_dock_log_line(
                shell,
                format!("[script {script_id} ERR] {description}"),
            );
        }
        glass_frida::SessionEvent::Detached { reason } => {
            push_dock_log_line(shell, format!("session detached: {reason}"));
        }
    }
}

/// Build an [`Invocation`] from the raw JSON the trace JS
/// sends. The shape is one of:
///   * `{kind: "call", args: [s1, s2, ...]}` — args are
///     pre-stringified by `safeRepr`.
///   * `{kind: "return", value: s}`.
///   * `{kind: "throw", error: s}` — uncaught exception.
///   * `{kind: "ready"}` — the script's setup completed.
///   * `{kind: "setup-error", error: s}` — the script
///     couldn't hook the method.
///
/// We collapse everything into a single-line summary so the
/// trace pane can render a clean column of text. Each variant
/// stores the kind tag (Call/Return) for grouping later.
fn invocation_from_payload(
    payload: &serde_json::Value,
) -> crate::traces::Invocation {
    use crate::traces::{Invocation, InvocationKind};
    let kind = payload
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or("");
    let (inv_kind, summary) = match kind {
        "call" => {
            let args: Vec<String> = payload
                .get("args")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|v| v.as_str().unwrap_or("?").to_string())
                        .collect()
                })
                .unwrap_or_default();
            (InvocationKind::Call, format!("call({})", args.join(", ")))
        }
        "return" => {
            let v = payload
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            (InvocationKind::Return, format!("→ {v}"))
        }
        "throw" => {
            let e = payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            (InvocationKind::Return, format!("↯ {e}"))
        }
        "ready" => (
            InvocationKind::Call,
            "(trace ready)".to_string(),
        ),
        "setup-info" => {
            // Diagnostic ping from the script's preamble.
            // Tells us whether Frida's Java bridge exists
            // at script-load time. Surfaces in the log so
            // the user can see "we got this far."
            let tj = payload
                .get("typeofJava")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let av = payload
                .get("available")
                .and_then(|v| v.as_bool())
                .map(|b| if b { "true" } else { "false" })
                .unwrap_or("?");
            (
                InvocationKind::Call,
                format!("(setup) typeof Java = {tj}, Java.available = {av}"),
            )
        }
        "info" => {
            // Generic diagnostic from a probe script. Just
            // dump the whole payload so we don't accidentally
            // hide a field. Used by the smoke-test diagnostic
            // path; can be reused by other probes.
            (InvocationKind::Call, format!("(info) {payload}"))
        }
        "wrapper-error" => {
            // Our own wrapper code threw (e.g. safeRepr on
            // a hostile object). Surfaced so the user sees
            // it without bringing down the app. `phase` is
            // either "format-args" or "format-return".
            let phase = payload
                .get("phase")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let err = payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            (
                InvocationKind::Call,
                format!("(wrapper-error {phase}) {err}"),
            )
        }
        "setup-error" => {
            // The error string can be quite long (a Java
            // stack trace) — render the whole thing so the
            // user can see what Frida actually rejected.
            // If the payload didn't have an `error` field,
            // fall back to dumping the raw JSON so we never
            // hide diagnostic info.
            let e = payload
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| payload.to_string());
            (InvocationKind::Call, format!("(setup error) {e}"))
        }
        _ => (InvocationKind::Call, payload.to_string()),
    };
    Invocation {
        at: std::time::Instant::now(),
        kind: inv_kind,
        summary,
    }
}

/// Append a line to the dock log. Wraps the borrow dance so
/// callers in this module can just pass a string.
fn push_dock_log_line(shell: &mut Shell, line: String) {
    if let Some(dock) = shell.debug_dock.as_mut() {
        dock.log.push(line);
        const MAX: usize = 200;
        if dock.log.len() > MAX {
            let drop = dock.log.len() - MAX;
            dock.log.drain(..drop);
        }
    }
}

fn spawn_flush_timer(shell: &gpui::Entity<Shell>, db: glass_db::Database, cx: &mut App) {
    let interval = db.flush_interval();
    let weak = shell.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(interval).await;
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

    let bg_progress = progress.clone();
    let loader_task = cx.background_executor().spawn(async move {
        load_bundle_blocking(path, bg_progress)
    });

    let weak = shell.downgrade();
    let progress_for_poll = progress.clone();
    cx.spawn(async move |cx| {
        let mut loader = Some(loader_task);
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(33))
                .await;

            let _ = weak.update(cx, |_s, cx| cx.notify());

            let done = progress_for_poll.lock().map(|p| p.done).unwrap_or(true);
            if done {
                break;
            }
        }

        let result = loader.take().expect("loader task").await;

        let _ = weak.update(cx, |shell, cx| {
            match result {
                Ok(mut bundle) => {
                    // Hydrate user annotations from the DB before
                    // handing the bundle to the rest of the Shell.
                    // The loader runs without DB access; this is the
                    // first point we have both the artifact list and
                    // the DB handle in the same place.
                    if let Some(db) = shell.db_ref() {
                        let mut idx = crate::annotations::load_for_artifacts(
                            db,
                            &bundle.artifact_ids,
                        );
                        // One-time upgrade of legacy `MethodLine`
                        // annotations to op-index keys. Runs while
                        // we still have both the DB handle and the
                        // freshly-lifted SmaliClass set; persists
                        // the result so subsequent opens skip the
                        // work (the upgrade is idempotent — no
                        // MethodLine entries means nothing to do).
                        crate::annotations::upgrade_method_line_to_op_index(
                            db,
                            &mut idx,
                            &bundle.smali_classes,
                        );
                        bundle.annotations = std::sync::Arc::new(idx);
                    }
                    shell.state = ShellState::Ready(bundle);
                }
                Err(e) => shell.state = ShellState::Error(format!("{e:#}")),
            }
            shell.progress = None;
            if let ShellState::Ready(b) = &shell.state {
                for (i, _) in b.tree.roots.iter().enumerate() {
                    shell.expanded.open.insert(vec![i]);
                }
                let bundle = b.clone();
                shell.restore_state(&bundle);
                shell.rebuild_list_state();
                shell.save_state();
                // Rebuild the macOS app menu so File → Open Recent
                // reflects the new ordering (this bundle just moved
                // to the top of the recent list during save_state).
                // Without this the menu labels go stale and clicking
                // "the same row" actually opens a different file —
                // the recent list reorders under the menu, but the
                // OpenRecentN action handlers re-query at click time
                // and pick up the post-reorder list.
                let db = shell.db.clone();
                set_app_menus(cx, db.as_ref());
                // Kick off xref-index builders. Each runs on its own
                // background task and writes results into the
                // bundle's XrefStore. The Shell renders progress
                // chips in scoped palettes while these build.
                spawn_xref_builders(&bundle, cx);
            }
            cx.notify();
        });
    })
    .detach();
}

/// Spawn the three xref-index builders on background tasks. Each
/// transitions the matching `XrefStore` slot through
/// Pending → Building(progress) → Ready(index).
fn spawn_xref_builders(bundle: &crate::LoadedBundle, cx: &mut App) {
    use crate::xref::{XrefIndexState, XrefProgress};
    use parking_lot::Mutex;

    // ---- DEX callers ---------------------------------------------
    // Scans every smali body to capture the line offset of each
    // `invoke-*` so palette entries can jump to the exact call
    // site. A few milliseconds even on huge DEX; runs on a
    // background task to match the field-refs cadence.
    {
        let slot = bundle.xrefs.dex_callers.clone();
        let bodies = bundle.bodies.clone();
        let kinds = bundle.kinds.clone();
        let progress = Arc::new(Mutex::new(XrefProgress {
            label: "DEX callers".to_string(),
            current: 0,
            total: kinds.iter().filter(|k| matches!(k, crate::LeafKind::SmaliClass { .. })).count(),
        }));
        *slot.write() = XrefIndexState::Building(progress.clone());
        cx.background_executor()
            .spawn(async move {
                let result =
                    crate::xref::build_dex_callers(&bodies, &kinds, &progress);
                *slot.write() = XrefIndexState::Ready(Arc::new(result));
            })
            .detach();
    }

    // ---- AArch64 xref index (the slow one) ----------------------
    // Walks every text section, decodes every instruction. 1-2s on
    // a 23 MB lib. The actual builder lands in a follow-up commit;
    // for now we transition straight to Ready(empty) so the slot is
    // queryable.
    {
        let slot = bundle.xrefs.native.clone();
        let text_sections = bundle.text_sections.clone();
        let data_sections = bundle.data_sections.clone();
        let progress = Arc::new(Mutex::new(XrefProgress {
            label: "Native references".to_string(),
            current: 0,
            total: text_sections
                .values()
                .map(|t| t.instruction_count())
                .sum(),
        }));
        *slot.write() = XrefIndexState::Building(progress.clone());
        cx.background_executor()
            .spawn(async move {
                let xrefs = crate::xref::build_native_xrefs(
                    &text_sections,
                    &data_sections,
                    &progress,
                );
                *slot.write() = XrefIndexState::Ready(Arc::new(xrefs));
            })
            .detach();
    }

    // ---- DEX field refs -----------------------------------------
    {
        let slot = bundle.xrefs.dex_field_refs.clone();
        let bodies = bundle.bodies.clone();
        let kinds = bundle.kinds.clone();
        let progress = Arc::new(Mutex::new(XrefProgress {
            label: "DEX field refs".to_string(),
            current: 0,
            total: kinds
                .iter()
                .filter(|k| matches!(k, crate::LeafKind::SmaliClass { .. }))
                .count(),
        }));
        *slot.write() = XrefIndexState::Building(progress.clone());
        cx.background_executor()
            .spawn(async move {
                let refs = crate::xref::build_dex_field_refs(&bodies, &kinds, &progress);
                *slot.write() = XrefIndexState::Ready(Arc::new(refs));
            })
            .detach();
    }
}
