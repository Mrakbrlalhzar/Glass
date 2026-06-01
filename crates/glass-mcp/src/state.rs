//! Process-wide state for the stateful MCP server.
//!
//! Lives behind a `Mutex` on `GlassHandler` and survives across
//! tool calls. Holds the currently-open bundle (if any) and —
//! once Phase B lands — the table of active Frida sessions.
//!
//! Why stateful: every previous verb call did
//! `glass_api::open(path)` fresh, re-parsing the bundle from
//! scratch. That doesn't scale to workflows like
//! "open → edit → edit → review → export" where the edits need
//! to accumulate, and Frida session control (attach → load →
//! poll → detach) is impossible without state outright.
//!
//! Lifetime: the MCP server is one stdio process per client
//! connection. State lives for that process's lifetime and is
//! dropped on disconnect. We don't persist anything to disk
//! here — bundles re-parse on the next session's `bundle-open`;
//! Frida sessions can't outlive the target process anyway.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

/// The single open bundle, when one is loaded. Held behind an
/// `Arc` so verbs can clone the handle out of the mutex and
/// run their work without holding the lock.
#[derive(Clone)]
pub(crate) struct OpenBundle {
    pub source_path: PathBuf,
    pub bundle: Arc<glass_api::Bundle>,
}

/// An attached Frida session and the metadata about how it
/// was set up. One per MCP connection — the GUI's Frida dock
/// follows the same one-session model.
pub(crate) struct FridaAttached {
    pub session: glass_frida::Session,
    pub host: String,
    pub pid: u32,
    pub agent_version: Option<String>,
    pub os: Option<String>,
}

/// Container for everything the MCP server needs to remember
/// across tool calls.
pub(crate) struct McpState {
    /// The currently-open bundle, if any. One at a time — most
    /// reversing workflows are scoped to a single app, and the
    /// few legitimate "compare two apps" flows can use two MCP
    /// connections. Single-bundle keeps the state shape simple.
    pub bundle: Option<OpenBundle>,
    /// The single attached Frida session, if any.
    pub frida: Option<FridaAttached>,
}

impl McpState {
    pub fn new() -> Self {
        Self {
            bundle: None,
            frida: None,
        }
    }

    /// Return the open bundle when its `source_path` matches
    /// `path`. Used by stateless verbs that take a `path` arg to
    /// avoid re-parsing when the same bundle is already loaded.
    pub fn bundle_for(&self, path: &Path) -> Option<OpenBundle> {
        let open = self.bundle.as_ref()?;
        // Compare via canonicalized paths when both exist so
        // `./app.apk` and `/abs/app.apk` are treated as the
        // same target. Fall back to lexical equality when
        // canonicalize fails (e.g. file deleted between calls).
        let same = match (
            open.source_path.canonicalize(),
            path.canonicalize(),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => open.source_path == path,
        };
        if same {
            Some(open.clone())
        } else {
            None
        }
    }
}

/// Cheap-to-clone handle to the shared state. The handler
/// stashes one of these on construction; dispatch arms clone it
/// into each tool-call's scope.
pub(crate) type StateHandle = Arc<Mutex<McpState>>;

pub(crate) fn new_state() -> StateHandle {
    Arc::new(Mutex::new(McpState::new()))
}
