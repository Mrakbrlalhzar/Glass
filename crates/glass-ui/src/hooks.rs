//! Live Frida method-hook registry.
//!
//! Sibling of [`crate::traces::TraceRegistry`] — same key
//! shape but a different intent: hooks **change** behaviour
//! (return values, args, side effects) rather than just
//! observing it. Each entry carries a user-authored JS
//! `body` that runs in place of the method's normal
//! implementation, plus the same ScriptId / status / bounded
//! invocation buffer the traces have.

use std::collections::HashMap;
use std::time::Instant;

pub use crate::traces::{Invocation, InvocationKind};

/// What kind of override the user wants. Stored alongside
/// `body` so the dialog can render a meaningful summary
/// ("returns true" vs "logs only" vs "custom JS").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAction {
    /// Run the method as-is; just record the call. Equivalent
    /// to a trace but tracked as a hook so the user can flip
    /// it to a return-override later without losing context.
    LogOnly,
    /// Skip the real implementation, return the embedded JS
    /// literal. We store it as a literal string and let
    /// `eval` produce the value at hook time. Examples:
    /// `true`, `42`, `"abc"`, `null`, `[1,2,3]`. For complex
    /// Java objects use `CustomJs`.
    ReturnLiteral(String),
    /// User-supplied JS body. Receives `args` and (optionally
    /// after calling `original.apply(this, args)`) `retval`;
    /// returns the value the caller will see. The body runs
    /// inside the wrapper so it has access to Frida's `Java.*`.
    CustomJs(String),
}

impl HookAction {
    /// Short display string for the dialog's action column.
    pub fn summary(&self) -> String {
        match self {
            HookAction::LogOnly => "log only".to_string(),
            HookAction::ReturnLiteral(lit) => {
                let trimmed = if lit.len() > 40 {
                    format!("{}…", &lit[..40])
                } else {
                    lit.clone()
                };
                format!("returns {trimmed}")
            }
            HookAction::CustomJs(_) => "custom JS".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookStatus {
    Pending,
    Active,
    Failed { message: String },
    Stopped,
}

/// Key — identical to TraceKey but in its own type so the
/// two registries don't accidentally collide. Hooks and
/// traces can coexist on the same method (the user might
/// run a trace, then add a hook on top); they own separate
/// scripts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HookKey {
    pub artifact: glass_db::ArtifactId,
    pub class_jni: String,
    pub method_name: String,
    pub method_signature: String,
}

#[derive(Debug, Clone)]
pub struct HookEntry {
    pub key: HookKey,
    pub script_id: Option<glass_frida::ScriptId>,
    pub status: HookStatus,
    pub action: HookAction,
    pub created_at: Instant,
    /// Most recent N invocations. Same bound as traces.
    pub invocations: Vec<Invocation>,
}

pub const MAX_INVOCATIONS_PER_HOOK: usize = 1000;

#[derive(Default, Debug, Clone)]
pub struct HookRegistry {
    by_key: HashMap<HookKey, HookEntry>,
    by_script: HashMap<glass_frida::ScriptId, HookKey>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn insert(&mut self, entry: HookEntry) {
        if let Some(id) = entry.script_id {
            self.by_script.insert(id, entry.key.clone());
        }
        self.by_key.insert(entry.key.clone(), entry);
    }

    pub fn get(&self, key: &HookKey) -> Option<&HookEntry> {
        self.by_key.get(key)
    }

    pub fn get_mut(&mut self, key: &HookKey) -> Option<&mut HookEntry> {
        self.by_key.get_mut(key)
    }

    pub fn key_for_script(&self, id: glass_frida::ScriptId) -> Option<&HookKey> {
        self.by_script.get(&id)
    }

    pub fn remove(&mut self, key: &HookKey) -> Option<HookEntry> {
        let entry = self.by_key.remove(key)?;
        if let Some(id) = entry.script_id {
            self.by_script.remove(&id);
        }
        Some(entry)
    }

    pub fn is_hooked(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        method_name: &str,
        method_signature: &str,
    ) -> bool {
        let probe = HookKey {
            artifact: artifact.clone(),
            class_jni: class_jni.to_string(),
            method_name: method_name.to_string(),
            method_signature: method_signature.to_string(),
        };
        match self.by_key.get(&probe) {
            Some(entry) => matches!(
                entry.status,
                HookStatus::Pending | HookStatus::Active
            ),
            None => false,
        }
    }

    pub fn entries(&self) -> Vec<&HookEntry> {
        self.by_key.values().collect()
    }

    pub fn push_invocation(&mut self, key: &HookKey, inv: Invocation) {
        let Some(entry) = self.by_key.get_mut(key) else { return };
        if entry.invocations.len() >= MAX_INVOCATIONS_PER_HOOK {
            let drop_count =
                entry.invocations.len() - MAX_INVOCATIONS_PER_HOOK + 1;
            entry.invocations.drain(..drop_count);
        }
        entry.invocations.push(inv);
    }

    pub fn mark_active(&mut self, key: &HookKey, script_id: glass_frida::ScriptId) {
        if let Some(entry) = self.by_key.get_mut(key) {
            entry.script_id = Some(script_id);
            entry.status = HookStatus::Active;
            self.by_script.insert(script_id, key.clone());
        }
    }

    pub fn mark_failed(&mut self, key: &HookKey, message: String) {
        if let Some(entry) = self.by_key.get_mut(key) {
            entry.status = HookStatus::Failed { message };
        }
    }

    pub fn clear(&mut self) {
        self.by_key.clear();
        self.by_script.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_key(method: &str) -> HookKey {
        HookKey {
            artifact: glass_db::ArtifactId::from_bytes(b"hookt"),
            class_jni: "Lcom/example/Foo;".into(),
            method_name: method.into(),
            method_signature: "()V".into(),
        }
    }

    fn mk_entry(key: HookKey, action: HookAction) -> HookEntry {
        HookEntry {
            key,
            script_id: None,
            status: HookStatus::Pending,
            action,
            created_at: Instant::now(),
            invocations: Vec::new(),
        }
    }

    #[test]
    fn insert_and_lookup() {
        let mut r = HookRegistry::new();
        let key = mk_key("bar");
        r.insert(mk_entry(key.clone(), HookAction::LogOnly));
        assert!(r.is_hooked(&key.artifact, &key.class_jni, "bar", "()V"));
    }

    #[test]
    fn action_summaries() {
        assert_eq!(HookAction::LogOnly.summary(), "log only");
        assert_eq!(
            HookAction::ReturnLiteral("true".into()).summary(),
            "returns true"
        );
        assert_eq!(HookAction::CustomJs("…".into()).summary(), "custom JS");
    }

    #[test]
    fn mark_active_then_route() {
        let mut r = HookRegistry::new();
        let key = mk_key("baz");
        r.insert(mk_entry(key.clone(), HookAction::LogOnly));
        r.mark_active(&key, 99);
        assert_eq!(r.key_for_script(99), Some(&key));
        assert!(matches!(r.get(&key).unwrap().status, HookStatus::Active));
    }
}
