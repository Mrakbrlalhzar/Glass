//! Hand-rolled GLib FFI declarations for the symbols Glass uses
//! directly (mostly main-loop pumping + GObject lifecycle around
//! Frida's `g_signal_connect_data`).
//!
//! Why we declare them ourselves: `frida-sys` runs `bindgen`
//! against `frida-core.h`, and the set of GLib symbols that
//! transitively appear in the generated bindings is
//! platform-dependent. On macOS the Frida devkit's bundled glib
//! pulls these through; on Linux the devkit relies on the
//! system's GLib for shared linkage and the header chain stops
//! at `<frida-core.h>`, leaving `g_main_context_default`,
//! `g_object_unref`, `g_clear_error`, `g_signal_connect_data`,
//! and friends undeclared in the Rust bindings.
//!
//! The symbols are present in the linked `libfrida-core`
//! (statically on macOS, via libglib-2.0 on Linux) — they're
//! just not in the Rust prototype set. Re-declaring our own
//! `extern "C"` prototypes is enough to compile, and the
//! linker resolves both decls (this one and the bindgen one)
//! to the same external symbol.
//!
//! `GError` is also re-declared so we have a single canonical
//! shape across platforms regardless of whether `frida-sys`
//! exposed it through its bindings or not. The Frida API
//! signatures take `*mut *mut GError`; pointers don't care
//! about distinct-but-identical struct definitions in Rust.

use std::ffi::{c_char, c_int, c_void};

/// GLib's `GError`. Layout matches `glib/gerror.h`. We keep our
/// own copy because some platforms' `frida-sys` bindings omit it
/// (see module docs). The Frida APIs that take `*mut *mut GError`
/// accept this struct verbatim via raw-pointer cast at the call
/// site — see `error_ptr` helpers below.
#[repr(C)]
#[derive(Debug)]
pub struct GError {
    pub domain: u32,
    pub code: c_int,
    pub message: *mut c_char,
}

/// Opaque GLib main-loop context. We never inspect its fields,
/// only pass it around by pointer.
#[repr(C)]
pub struct GMainContext {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    /// Returns the default `GMainContext` for the calling thread.
    /// Frida's worker thread uses this; pumping it from our own
    /// thread drives Frida's async callbacks.
    pub fn g_main_context_default() -> *mut GMainContext;

    /// `true` if `ctx` has work pending. Cheap; safe to poll.
    pub fn g_main_context_pending(ctx: *mut GMainContext) -> c_int;

    /// Run one iteration of `ctx`'s event loop. `may_block`
    /// controls whether the call may park the thread waiting for
    /// new events. We always pass `false` (poll-only).
    pub fn g_main_context_iteration(ctx: *mut GMainContext, may_block: c_int) -> c_int;

    /// `g_object_unref` — drop a reference on a GObject. Used to
    /// release `FridaScriptOptions` etc. once the API call that
    /// consumed them has returned.
    pub fn g_object_unref(object: *mut c_void);

    /// Free a `GError` and null out the pointer. Standard GLib
    /// error-cleanup helper.
    pub fn g_clear_error(err: *mut *mut GError);

    /// Connect a signal handler. We use this for the `message`
    /// signal on `FridaScript`. `handler` is type-erased through
    /// `Option<unsafe extern "C" fn()>` because GClosure's calling
    /// convention varies per signal.
    pub fn g_signal_connect_data(
        instance: *mut c_void,
        detailed_signal: *const c_char,
        c_handler: Option<unsafe extern "C" fn()>,
        data: *mut c_void,
        destroy_data: Option<unsafe extern "C" fn(data: *mut c_void, closure: *mut c_void)>,
        connect_flags: c_int,
    ) -> u64;
}

/// Cast our `*mut *mut GError` to whatever pointer-to-pointer
/// shape `frida-sys`'s bindgen output landed on for this build —
/// the struct layout is identical but the Rust type name differs
/// across platforms (and may not even be `frida_sys::GError` on
/// some Linux devkits). The Frida API signatures specify the
/// pointer type via type inference at the call site, so going
/// through `*mut c_void` and casting works on every platform.
#[inline]
pub fn err_out<T>(p: &mut *mut GError) -> *mut *mut T {
    p as *mut *mut GError as *mut *mut T
}
