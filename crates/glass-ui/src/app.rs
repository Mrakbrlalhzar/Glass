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
    prelude::*, px, size, App, Bounds, Context, KeyBinding, Window, WindowBounds, WindowOptions,
};
use gpui_platform::application;

use crate::loader::load_bundle_blocking;
use crate::{
    CloseWindow, NewWindow, OpenFile, OpenRecent0, OpenRecent1, OpenRecent2, OpenRecent3,
    OpenRecent4, OpenRecent5, OpenRecent6, OpenRecent7, OpenRecent8, OpenRecent9,
    PaletteActivate, PaletteClose, PaletteDown, PaletteUp, Progress, Quit, Shell, ShellState,
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
            shell.update(cx, |_shell, cx: &mut Context<Shell>| {
                cx.observe_window_bounds(window, |_shell, window: &mut Window, _cx| {
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
                Ok(bundle) => shell.state = ShellState::Ready(bundle),
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
            }
            cx.notify();
        });
    })
    .detach();
}
