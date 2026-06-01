//! Long-lived Frida session as an actor.
//!
//! The underlying `frida::Session` and every `frida::Script`
//! built from it hold raw pointers and `RefCell`s — they're
//! `!Send + !Sync`. So we can't hand them out to the GUI; we
//! own them on a dedicated background thread and talk to that
//! thread via channels.
//!
//! The thread is also responsible for pumping GLib's main
//! context so frida-core's signal callbacks (`message`, etc.)
//! actually fire. Without that pump, scripts can call
//! `send(…)` from the device but the host never sees the
//! event.
//!
//! Public surface from the GUI's side:
//!
//!   * [`Session`] — a cheap clone-able handle. Holds the
//!     command sender + a shared event receiver.
//!   * [`Session::attach_remote`] — establish a connection
//!     to a frida-gadget at `host:port` (e.g. an `adb forward`
//!     loopback).
//!   * [`Session::create_script`] — load JS into the gadget;
//!     returns a [`ScriptId`] for routing messages.
//!   * [`Session::unload_script`] — clean shutdown of a
//!     specific script.
//!   * [`Session::poll_event`] — non-blocking drain of any
//!     events the actor has accumulated; called by the GUI's
//!     tick loop.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread;
use std::time::Duration;

/// Identifier for a loaded script. Lets the GUI route a
/// `ScriptMessage` event back to the trace / inspect / hook
/// that originated it. Allocated by the GUI side so the actor
/// can echo it back on every message; cheap u32.
pub type ScriptId = u32;

/// A command the GUI sends to the actor. All commands carry a
/// oneshot reply channel where applicable so the caller can
/// await success or failure synchronously.
enum Command {
    AttachRemote {
        host: String,
        pid: u32,
        reply: mpsc::Sender<Result<AttachReport, String>>,
    },
    CreateScript {
        id: ScriptId,
        name: String,
        source: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
    UnloadScript {
        id: ScriptId,
        reply: mpsc::Sender<Result<(), String>>,
    },
    PostMessage {
        id: ScriptId,
        message: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Detach {
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Ask the gadget to resume — only meaningful when the
    /// gadget was configured with `on_load: wait` and is
    /// currently blocked inside <clinit>. Wraps
    /// frida_device_resume_sync against the attached
    /// device. Cheap no-op if the gadget has already
    /// resumed (frida-core returns OK).
    Resume {
        pid: u32,
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Tell the actor to shut down cleanly. Drops everything,
    /// then exits the loop. Used when the dock closes.
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct AttachReport {
    /// Frida agent version reported by the gadget. Empty
    /// when frida-core didn't surface one.
    pub agent_version: Option<String>,
    /// OS string from the device — informational.
    pub os: Option<String>,
}

/// Events the actor produces. The GUI polls these via
/// `poll_event` on its tick.
#[derive(Debug)]
pub enum SessionEvent {
    /// A `send(...)` call from a loaded script. `payload` is
    /// the raw JSON value the script passed; the GUI's
    /// handler decodes it according to which feature owns
    /// `script_id`.
    ScriptMessage {
        script_id: ScriptId,
        payload: serde_json::Value,
    },
    /// A runtime error inside the JS — usually a typo or a
    /// stale reference. Surfaced so the dock can show it.
    ScriptError {
        script_id: ScriptId,
        description: String,
    },
    /// Frida log line (Console.log inside the script).
    ScriptLog {
        script_id: ScriptId,
        level: String,
        message: String,
    },
    /// Session detached unexpectedly (device unplugged, app
    /// crashed, gadget killed). After this no further calls
    /// will succeed; the GUI should reset state.
    Detached { reason: String },
}

/// Cheap clone-able handle. The actor thread + its frida
/// state live behind the channels; this struct is just the
/// reply-address pair.
#[derive(Clone, Debug)]
pub struct Session {
    cmd_tx: mpsc::Sender<Command>,
    event_rx: Arc<Mutex<mpsc::Receiver<SessionEvent>>>,
    next_script_id: Arc<AtomicU32>,
}

impl Session {
    /// Spawn the actor and return a handle. The actor thread
    /// stays alive until [`Session::shutdown`] (or every
    /// handle is dropped).
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>();
        thread::Builder::new()
            .name("glass-frida-actor".to_string())
            .spawn(move || actor_main(cmd_rx, event_tx))
            .expect("spawn glass-frida-actor thread");
        Self {
            cmd_tx,
            event_rx: Arc::new(Mutex::new(event_rx)),
            next_script_id: Arc::new(AtomicU32::new(1)),
        }
    }

    /// Allocate a fresh script id. The GUI passes this in to
    /// [`create_script`] so it can correlate later message
    /// events back to the feature that owns the script.
    pub fn alloc_script_id(&self) -> ScriptId {
        self.next_script_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn attach_remote(
        &self,
        host: impl Into<String>,
        pid: u32,
    ) -> Result<AttachReport, String> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(Command::AttachRemote {
                host: host.into(),
                pid,
                reply: tx,
            })
            .map_err(|_| "session actor dead".to_string())?;
        rx.recv().map_err(|_| "actor dropped reply".to_string())?
    }

    pub fn create_script(
        &self,
        id: ScriptId,
        name: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(Command::CreateScript {
                id,
                name: name.into(),
                source: source.into(),
                reply: tx,
            })
            .map_err(|_| "session actor dead".to_string())?;
        rx.recv().map_err(|_| "actor dropped reply".to_string())?
    }

    pub fn unload_script(&self, id: ScriptId) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(Command::UnloadScript { id, reply: tx })
            .map_err(|_| "session actor dead".to_string())?;
        rx.recv().map_err(|_| "actor dropped reply".to_string())?
    }

    /// Post a JSON message to a loaded script. The script
    /// observes it via `recv(...)`.
    pub fn post_message(
        &self,
        id: ScriptId,
        message: impl Into<String>,
    ) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(Command::PostMessage {
                id,
                message: message.into(),
                reply: tx,
            })
            .map_err(|_| "session actor dead".to_string())?;
        rx.recv().map_err(|_| "actor dropped reply".to_string())?
    }

    /// Unblock a gadget that was loaded with `on_load: wait`.
    /// Called from Glass's Restart orchestrator after every
    /// trace/hook has been re-installed against the paused
    /// process.
    pub fn resume(&self, pid: u32) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(Command::Resume { pid, reply: tx })
            .map_err(|_| "session actor dead".to_string())?;
        rx.recv().map_err(|_| "actor dropped reply".to_string())?
    }

    pub fn detach(&self) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(Command::Detach { reply: tx })
            .map_err(|_| "session actor dead".to_string())?;
        rx.recv().map_err(|_| "actor dropped reply".to_string())?
    }

    /// Drain whatever events the actor has accumulated since
    /// the last call. Non-blocking; returns `[]` when nothing
    /// is ready.
    pub fn poll_events(&self) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        if let Ok(rx) = self.event_rx.lock() {
            while let Ok(ev) = rx.try_recv() {
                out.push(ev);
            }
        }
        out
    }

    /// Ask the actor to shut down. The thread exits its loop;
    /// further command attempts return "session actor dead".
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
    }
}

// ---- Actor thread ----------------------------------------------------------

fn actor_main(
    cmd_rx: mpsc::Receiver<Command>,
    event_tx: mpsc::Sender<SessionEvent>,
) {
    use std::collections::HashMap;

    // Touch the runtime so frida-core is initialised on this
    // thread (Frida::obtain is a singleton — first call wins,
    // subsequent calls are cheap).
    let _rt = match crate::FridaRuntime::get() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = event_tx.send(SessionEvent::Detached {
                reason: format!("frida runtime not built: {e:?}"),
            });
            // Still drain the command channel so callers see
            // the error rather than hanging.
            drain_commands_with_error(cmd_rx, "frida runtime not initialised");
            return;
        }
    };

    // Hold all live state on the actor thread. None of these
    // are Send; that's fine, they never leave this scope.
    // `'static`-erased via `*mut _` underneath; we own the
    // raw pointers through frida-rust wrappers.
    let mgr_holder = ManagerHolder::new();
    let mut session_holder: Option<SessionHolder> = None;
    let mut scripts: HashMap<ScriptId, ScriptHolder> = HashMap::new();

    loop {
        // Drain any pending commands (non-blocking). This
        // way the actor processes commands as they arrive
        // without sleeping; we only sleep between idle
        // ticks.
        let mut got_cmd = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    got_cmd = true;
                    if !handle_command(
                        cmd,
                        &mgr_holder,
                        &mut session_holder,
                        &mut scripts,
                        &event_tx,
                    ) {
                        // Shutdown requested.
                        return;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // All handles dropped — exit cleanly.
                    return;
                }
            }
        }

        // Pump GLib's main context so frida-core's signal
        // callbacks (message handlers) fire. `false` for
        // the may_block argument — we don't want to block
        // here; the inner loop handles backlog quickly and
        // then we sleep below.
        unsafe {
            let ctx = frida_sys::g_main_context_default();
            while frida_sys::g_main_context_pending(ctx) != 0 {
                frida_sys::g_main_context_iteration(ctx, 0);
            }
        }

        if !got_cmd {
            // Nothing happened this tick — short sleep
            // to keep CPU low. GLib's pump is cheap when
            // nothing's pending, so 10ms is a fine
            // tradeoff between responsiveness and idle
            // load.
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn drain_commands_with_error(cmd_rx: mpsc::Receiver<Command>, msg: &str) {
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Command::Shutdown => return,
            Command::AttachRemote { reply, .. } => {
                let _ = reply.send(Err(msg.to_string()));
            }
            Command::CreateScript { reply, .. } => {
                let _ = reply.send(Err(msg.to_string()));
            }
            Command::UnloadScript { reply, .. } => {
                let _ = reply.send(Err(msg.to_string()));
            }
            Command::PostMessage { reply, .. } => {
                let _ = reply.send(Err(msg.to_string()));
            }
            Command::Detach { reply } => {
                let _ = reply.send(Err(msg.to_string()));
            }
            Command::Resume { reply, .. } => {
                let _ = reply.send(Err(msg.to_string()));
            }
        }
    }
}

// ---- Holders --------------------------------------------------------------
//
// These structs exist purely to manage the lifetime of frida
// objects on the actor thread. The frida-rust wrappers carry
// lifetimes we can't satisfy across actor iterations, so we
// store raw pointers + own the drop ourselves.

struct ManagerHolder {
    ptr: *mut frida_sys::_FridaDeviceManager,
}

impl ManagerHolder {
    fn new() -> Self {
        let ptr = unsafe { frida_sys::frida_device_manager_new() };
        Self { ptr }
    }
}

impl Drop for ManagerHolder {
    fn drop(&mut self) {
        unsafe {
            frida_sys::frida_device_manager_close_sync(
                self.ptr,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            frida_sys::frida_unref(self.ptr as _);
        }
    }
}

struct SessionHolder {
    ptr: *mut frida_sys::_FridaSession,
    device_ptr: *mut frida_sys::_FridaDevice,
}

impl Drop for SessionHolder {
    fn drop(&mut self) {
        unsafe {
            // Don't error-check — we're tearing down on a
            // best-effort basis.
            frida_sys::frida_session_detach_sync(
                self.ptr,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            frida_sys::frida_unref(self.ptr as _);
            frida_sys::frida_unref(self.device_ptr as _);
        }
    }
}

struct ScriptHolder {
    ptr: *mut frida_sys::_FridaScript,
    /// Boxed callback context. Kept alive as long as the
    /// script is — g_signal_connect_data holds a raw pointer
    /// to it.
    _callback: Box<ScriptCallback>,
}

impl Drop for ScriptHolder {
    fn drop(&mut self) {
        unsafe {
            frida_sys::frida_script_unload_sync(
                self.ptr,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            frida_sys::frida_unref(self.ptr as _);
        }
    }
}

struct ScriptCallback {
    script_id: ScriptId,
    tx: mpsc::Sender<SessionEvent>,
}

// ---- Command handling ----------------------------------------------------

fn handle_command(
    cmd: Command,
    mgr: &ManagerHolder,
    session: &mut Option<SessionHolder>,
    scripts: &mut std::collections::HashMap<ScriptId, ScriptHolder>,
    event_tx: &mpsc::Sender<SessionEvent>,
) -> bool {
    use std::ffi::CString;

    match cmd {
        Command::Shutdown => return false,
        Command::AttachRemote { host, pid, reply } => {
            // Drop any prior session.
            *session = None;
            scripts.clear();
            let host_c = match CString::new(host.clone()) {
                Ok(c) => c,
                Err(_) => {
                    let _ = reply.send(Err("invalid host string".into()));
                    return true;
                }
            };
            unsafe {
                let mut err: *mut frida_sys::GError = std::ptr::null_mut();
                let device = frida_sys::frida_device_manager_add_remote_device_sync(
                    mgr.ptr,
                    host_c.as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut err,
                );
                if !err.is_null() || device.is_null() {
                    let _ = reply.send(Err(format!(
                        "add_remote_device {host}: error"
                    )));
                    if !err.is_null() {
                        frida_sys::g_clear_error(&mut err);
                    }
                    return true;
                }
                let mut err: *mut frida_sys::GError = std::ptr::null_mut();
                let sess_ptr = frida_sys::frida_device_attach_sync(
                    device,
                    pid,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut err,
                );
                if !err.is_null() || sess_ptr.is_null() {
                    frida_sys::frida_unref(device as _);
                    if !err.is_null() {
                        frida_sys::g_clear_error(&mut err);
                    }
                    let _ = reply.send(Err(format!("attach pid {pid}: error")));
                    return true;
                }
                *session = Some(SessionHolder {
                    ptr: sess_ptr,
                    device_ptr: device,
                });
            }
            let _ = reply.send(Ok(AttachReport {
                agent_version: None,
                os: None,
            }));
        }
        Command::CreateScript {
            id,
            name,
            source,
            reply,
        } => {
            let Some(sess) = session.as_ref() else {
                let _ = reply.send(Err("not attached".into()));
                return true;
            };
            let source_c = match CString::new(source) {
                Ok(c) => c,
                Err(_) => {
                    let _ = reply.send(Err("script source contained NUL".into()));
                    return true;
                }
            };
            let name_c = match CString::new(name) {
                Ok(c) => c,
                Err(_) => {
                    let _ = reply.send(Err("script name contained NUL".into()));
                    return true;
                }
            };
            unsafe {
                let opts = frida_sys::frida_script_options_new();
                frida_sys::frida_script_options_set_name(opts, name_c.as_ptr());
                // Leave runtime at DEFAULT. Overriding to
                // V8 turned out to *break* the Java bridge
                // in gadget mode (the gadget pre-wires the
                // bridge into its embedded runtime; asking
                // for a different runtime spins up a fresh
                // isolate without `Java` defined). The
                // default — whatever the gadget was compiled
                // against — is the right choice.
                frida_sys::frida_script_options_set_runtime(
                    opts,
                    frida_sys::FridaScriptRuntime_FRIDA_SCRIPT_RUNTIME_DEFAULT,
                );
                let mut err: *mut frida_sys::GError = std::ptr::null_mut();
                let script_ptr = frida_sys::frida_session_create_script_sync(
                    sess.ptr,
                    source_c.as_ptr(),
                    opts,
                    std::ptr::null_mut(),
                    &mut err,
                );
                frida_sys::g_object_unref(opts as _);
                if !err.is_null() || script_ptr.is_null() {
                    // Surface the GError message so the dock
                    // log shows what frida-core actually
                    // rejected. Common causes: source that
                    // triggers an immediate JS parse error
                    // inside the gadget, or a script name
                    // collision.
                    let msg = if !err.is_null() {
                        let m = (*err).message;
                        let owned = if m.is_null() {
                            "create_script: unknown error".to_string()
                        } else {
                            std::ffi::CStr::from_ptr(m as *const _)
                                .to_string_lossy()
                                .into_owned()
                        };
                        frida_sys::g_clear_error(&mut err);
                        owned
                    } else {
                        "create_script returned null script_ptr".to_string()
                    };
                    let _ = reply.send(Err(msg));
                    return true;
                }
                // Register the message callback.
                let cb = Box::new(ScriptCallback {
                    script_id: id,
                    tx: event_tx.clone(),
                });
                let cb_ptr: *const ScriptCallback = &*cb;
                let signal_name = std::ffi::CString::new("message").unwrap();
                let callback_fn: unsafe extern "C" fn() = std::mem::transmute(
                    on_script_message as *const (),
                );
                frida_sys::g_signal_connect_data(
                    script_ptr as _,
                    signal_name.as_ptr(),
                    Some(callback_fn),
                    cb_ptr as *mut std::ffi::c_void,
                    None,
                    0,
                );
                // Now load it.
                let mut err: *mut frida_sys::GError = std::ptr::null_mut();
                frida_sys::frida_script_load_sync(
                    script_ptr,
                    std::ptr::null_mut(),
                    &mut err,
                );
                if !err.is_null() {
                    let m = (*err).message;
                    let msg = if m.is_null() {
                        "script load: unknown error".to_string()
                    } else {
                        std::ffi::CStr::from_ptr(m as *const _)
                            .to_string_lossy()
                            .into_owned()
                    };
                    frida_sys::g_clear_error(&mut err);
                    frida_sys::frida_unref(script_ptr as _);
                    let _ = reply.send(Err(format!("script load: {msg}")));
                    return true;
                }
                scripts.insert(
                    id,
                    ScriptHolder {
                        ptr: script_ptr,
                        _callback: cb,
                    },
                );
            }
            let _ = reply.send(Ok(()));
        }
        Command::UnloadScript { id, reply } => {
            scripts.remove(&id);
            let _ = reply.send(Ok(()));
        }
        Command::PostMessage { id, message, reply } => {
            let Some(script) = scripts.get(&id) else {
                let _ = reply.send(Err("script not loaded".into()));
                return true;
            };
            let msg_c = match CString::new(message) {
                Ok(c) => c,
                Err(_) => {
                    let _ = reply.send(Err("message contained NUL".into()));
                    return true;
                }
            };
            unsafe {
                frida_sys::frida_script_post(
                    script.ptr,
                    msg_c.as_ptr(),
                    std::ptr::null_mut(),
                );
            }
            let _ = reply.send(Ok(()));
        }
        Command::Detach { reply } => {
            scripts.clear();
            *session = None;
            let _ = reply.send(Ok(()));
        }
        Command::Resume { pid, reply } => {
            let Some(sess) = session.as_ref() else {
                let _ = reply.send(Err("not attached".into()));
                return true;
            };
            // frida_device_resume_sync against the device we
            // attached to. Cheap; the gadget unblocks the
            // <clinit> wait loop. Already-resumed devices
            // return success.
            unsafe {
                let mut err: *mut frida_sys::GError = std::ptr::null_mut();
                frida_sys::frida_device_resume_sync(
                    sess.device_ptr,
                    pid,
                    std::ptr::null_mut(),
                    &mut err,
                );
                if !err.is_null() {
                    let msg_ptr = (*err).message;
                    let msg = if msg_ptr.is_null() {
                        "resume failed".to_string()
                    } else {
                        std::ffi::CStr::from_ptr(msg_ptr as *const _)
                            .to_string_lossy()
                            .into_owned()
                    };
                    frida_sys::g_clear_error(&mut err);
                    let _ = reply.send(Err(msg));
                    return true;
                }
            }
            let _ = reply.send(Ok(()));
        }
    }
    true
}

// GLib `message` signal handler. Frida passes the JSON
// message + optional binary data. We decode the JSON, route
// it to the right event variant, and push onto the channel.
unsafe extern "C" fn on_script_message(
    _script: *mut frida_sys::_FridaScript,
    message: *const i8,
    _data: *const frida_sys::_GBytes,
    user_data: *mut std::ffi::c_void,
) {
    if user_data.is_null() || message.is_null() {
        return;
    }
    let cb = &*(user_data as *const ScriptCallback);
    let c_msg = match std::ffi::CStr::from_ptr(message as *const std::ffi::c_char).to_str() {
        Ok(s) => s,
        Err(_) => return,
    };
    let parsed: serde_json::Value = match serde_json::from_str(c_msg) {
        Ok(v) => v,
        Err(e) => {
            let _ = cb.tx.send(SessionEvent::ScriptError {
                script_id: cb.script_id,
                description: format!("malformed script message: {e}"),
            });
            return;
        }
    };
    // Frida's wire format: { type: "send" | "log" | "error", ... }
    let kind = parsed
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("send");
    match kind {
        "send" => {
            let payload = parsed
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let _ = cb.tx.send(SessionEvent::ScriptMessage {
                script_id: cb.script_id,
                payload,
            });
        }
        "log" => {
            let level = parsed
                .get("level")
                .and_then(|l| l.as_str())
                .unwrap_or("info")
                .to_string();
            let msg = parsed
                .get("payload")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            let _ = cb.tx.send(SessionEvent::ScriptLog {
                script_id: cb.script_id,
                level,
                message: msg,
            });
        }
        "error" => {
            let desc = parsed
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("(no description)")
                .to_string();
            let _ = cb.tx.send(SessionEvent::ScriptError {
                script_id: cb.script_id,
                description: desc,
            });
        }
        _ => {
            // Forward as a Message anyway so we don't lose
            // anything unusual.
            let _ = cb.tx.send(SessionEvent::ScriptMessage {
                script_id: cb.script_id,
                payload: parsed,
            });
        }
    }
}
