//! glass-script: QuickJS-backed plugin runtime (M3).
//!
//! Stub for M0 — establishes the crate boundary so the `glass` JS API
//! surface can be designed in isolation from the UI.

pub struct ScriptHost;

impl ScriptHost {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ScriptHost {
    fn default() -> Self {
        Self::new()
    }
}
