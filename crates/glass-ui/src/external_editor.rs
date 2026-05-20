//! Live link to an external editor session over one smali class.
//!
//! Flow:
//!   1. User clicks "Edit File" on the toolbar (only enabled when
//!      the active tab is a smali class).
//!   2. We resolve the staged-or-original `SmaliClass`, render it
//!      via `to_smali()`, and write it to a temp `.smali` file
//!      under `$TMPDIR/glass-external/`.
//!   3. We spawn the OS's registered editor (macOS: `open
//!      <path>` — without `-W`, so we don't block on the editor
//!      process; the editor might already be running or might
//!      detach into the background regardless).
//!   4. A background polling task watches the temp file's mtime.
//!      Every change re-reads + parses + stages the result —
//!      successful saves silently update the in-memory bundle,
//!      bad saves leave the previous staged version intact and
//!      surface the error on the toolbar chip.
//!   5. The user clicks the toolbar chip to stop watching when
//!      they're done. Closing the editor window doesn't need to
//!      signal anything; we already saw the last save.
//!
//! Why polling not fsevents/inotify: the only requirement is
//! "detect that the user pressed Cmd-S", which doesn't need to be
//! instantaneous. Polling at ~500ms is cheap (one stat call per
//! tick), needs no new dependency, and works identically on every
//! platform we ship.

use std::path::PathBuf;
use std::time::SystemTime;

use gpui::{div, prelude::*, px, Context, SharedString};

use crate::Shell;

/// State for a live external-edit session. Lives on
/// `Shell.external_edit` from the moment the user clicks Edit
/// File until they click Stop on the toolbar chip.
pub struct ExternalEditState {
    pub artifact: glass_db::ArtifactId,
    pub class_jni: String,
    pub class_display: String,
    pub temp_path: PathBuf,
    /// The last mtime we successfully ingested. The poll task
    /// compares against this to decide whether to re-read.
    pub last_mtime: SystemTime,
    /// Last parse error, if any. `Some` means the most recent
    /// save didn't parse; previous staged version is unchanged.
    pub last_error: Option<String>,
    /// Flipped by the foreground when the user clicks Stop. The
    /// poll loop checks this each tick and exits cleanly.
    pub stop_requested: bool,
}

/// Whether the toolbar button should be visible. The Edit File
/// chip stays available even while a session is active — clicking
/// it on the same class is a no-op, on a different class it
/// switches the watcher. (For v1 we keep it strict: hidden while
/// any session is active.)
pub fn can_open_editor(shell: &Shell) -> bool {
    if shell.external_edit.is_some() {
        return false;
    }
    let Some(active) = shell.active_tab else { return false };
    matches!(
        shell.tabs.get(active).map(|t| &t.kind),
        Some(crate::TabKind::SmaliClass { .. })
    )
}

/// Render the toolbar chip for an active session. Returned by the
/// header builder when `shell.external_edit.is_some()`.
pub fn render_chip(
    state: &ExternalEditState,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let theme = crate::theme::current();
    let has_error = state.last_error.is_some();
    let border = if has_error {
        theme.errors.highlight.rgba()
    } else {
        theme.state.committed_change.rgba()
    };
    let bg = if has_error {
        // Tinted-red panel — derived from `errors.highlight` at
        // low alpha so the toolbar still reads.
        let h = theme.errors.highlight.rgba();
        gpui::Rgba { r: h.r, g: h.g, b: h.b, a: 0.18 }
    } else {
        theme.state.committed_bg.rgba()
    };
    let hover_bg = if has_error {
        gpui::Rgba { r: 1.0, g: 0.4, b: 0.4, a: 0.28 }
    } else {
        theme.state.committed_hover.rgba()
    };
    // Tooltip text. Errors go inline with the chip label so we
    // don't depend on hover for the key signal.
    let primary = if has_error {
        format!("× Parse error: {}", state.class_display)
    } else {
        format!("Editing {} externally", state.class_display)
    };
    let secondary = state
        .last_error
        .clone()
        .unwrap_or_else(|| "click to stop watching".to_string());
    div()
        .id("external-edit-chip")
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
        .bg(bg)
        .hover(move |s| s.bg(hover_bg))
        .cursor_pointer()
        .child(SharedString::from(primary))
        .child(
            div()
                .text_xs()
                .text_color(dim)
                .child(SharedString::from(secondary)),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                this.stop_external_edit_watch(cx);
            }),
        )
}

/// Write `body` to a temp file under `$TMPDIR/glass-external/`.
/// The filename encodes the JNI signature (with slashes folded to
/// dots) so it's recognisable on disk while still being a valid
/// filename on every platform.
pub fn write_temp_file(
    class_jni: &str,
    body: &str,
) -> std::io::Result<PathBuf> {
    let mut dir = std::env::temp_dir();
    dir.push("glass-external");
    std::fs::create_dir_all(&dir)?;
    let safe = class_jni
        .trim_start_matches('L')
        .trim_end_matches(';')
        .replace('/', ".");
    let mut path = dir;
    path.push(format!("{safe}.smali"));
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Launch the OS's registered editor on `path`. Doesn't wait for
/// the editor process — file-watching is how we detect saves.
/// Returns immediately, before the editor window appears.
pub fn launch_editor(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn().map(|_| ())
    }
    #[cfg(not(target_os = "macos"))]
    {
        // v1 ships macOS-only — surface a clear error so the caller
        // can show it on the chip.
        let _ = path;
        Err(std::io::Error::other(
            "external editor launching is only implemented on macOS for now",
        ))
    }
}

/// Read the mtime of `path`. Returns `SystemTime::UNIX_EPOCH` on
/// failure so the caller doesn't have to deal with `Option` —
/// missing-file or stat-error both mean "no change to apply".
pub fn mtime(path: &std::path::Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}
