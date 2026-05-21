//! Live Frida method-trace registry.
//!
//! Sibling of [`crate::smali_edits::SmaliEditRegistry`] — same
//! shape, different content. Each entry represents a method we've
//! asked the gadget to instrument, keyed by `(artifact, class,
//! method, signature)`. Holds the [`glass_frida::ScriptId`] so we
//! can route message events back, plus a bounded ring of recent
//! invocations the dock's trace pane renders.
//!
//! Stays in-memory like `smali_edits`. Closing the bundle (or
//! disconnecting the dock) drops every trace; the actor unloads
//! the underlying scripts when the session shuts down.

use std::collections::HashMap;
use std::time::Instant;

/// Identifies one traced method. We key by signature too because
/// Java overloads share a name but mean different things at the
/// bytecode level.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TraceKey {
    pub artifact: glass_db::ArtifactId,
    /// JNI signature of the class: `Lcom/example/Foo;`.
    pub class_jni: String,
    /// Bare method name (no signature). `<init>` for constructors,
    /// `<clinit>` for static init.
    pub method_name: String,
    /// JNI method signature, e.g. `(Ljava/lang/String;I)V`.
    pub method_signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceStatus {
    /// Script has been created on the host but `load_sync`
    /// hasn't returned yet.
    Pending,
    /// Script is loaded; gadget is reporting invocations.
    Active,
    /// Script load failed. `message` carries the error.
    Failed { message: String },
    /// User stopped the trace. The registry entry stays so the
    /// invocations are still visible in the pane, but no new
    /// events will arrive.
    Stopped,
}

/// One captured method invocation. Built from a `ScriptMessage`
/// arriving on the session event channel.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// Host-side timestamp the event arrived. Used for the
    /// pane's time column. Device-side timestamps would be more
    /// accurate but require an extra round trip; host monotonic
    /// is fine for ordering and human-scale gaps.
    pub at: Instant,
    /// `"call"` for entry, `"return"` for exit. We keep them as
    /// separate events so the user sees both sides; the pane
    /// can group them visually by depth/index.
    pub kind: InvocationKind,
    /// Pre-rendered summary line — one-line view of args or
    /// return value. Built from the raw JSON the gadget sends.
    /// Stored pre-rendered to avoid re-stringifying on every
    /// repaint of the pane.
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationKind {
    Call,
    Return,
}

#[derive(Debug, Clone)]
pub struct TraceEntry {
    pub key: TraceKey,
    /// Script the gadget is running for this trace. None until
    /// `start_trace` finishes the actor round-trip.
    pub script_id: Option<glass_frida::ScriptId>,
    pub status: TraceStatus,
    pub created_at: Instant,
    /// Bounded ring buffer of recent invocations. We cap at
    /// `MAX_INVOCATIONS_PER_TRACE` so a chatty method (e.g.
    /// onTouch) can't OOM the dock.
    pub invocations: Vec<Invocation>,
}

pub const MAX_INVOCATIONS_PER_TRACE: usize = 1000;

#[derive(Default, Debug, Clone)]
pub struct TraceRegistry {
    by_key: HashMap<TraceKey, TraceEntry>,
    /// Reverse index — given a script_id from a SessionEvent we
    /// look up the trace it belongs to in O(1).
    by_script: HashMap<glass_frida::ScriptId, TraceKey>,
}

impl TraceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn insert(&mut self, entry: TraceEntry) {
        if let Some(id) = entry.script_id {
            self.by_script.insert(id, entry.key.clone());
        }
        self.by_key.insert(entry.key.clone(), entry);
    }

    pub fn get(&self, key: &TraceKey) -> Option<&TraceEntry> {
        self.by_key.get(key)
    }

    pub fn get_mut(&mut self, key: &TraceKey) -> Option<&mut TraceEntry> {
        self.by_key.get_mut(key)
    }

    /// Look up by ScriptId — used when a SessionEvent arrives.
    pub fn key_for_script(&self, id: glass_frida::ScriptId) -> Option<&TraceKey> {
        self.by_script.get(&id)
    }

    pub fn remove(&mut self, key: &TraceKey) -> Option<TraceEntry> {
        let entry = self.by_key.remove(key)?;
        if let Some(id) = entry.script_id {
            self.by_script.remove(&id);
        }
        Some(entry)
    }

    /// Whether the given method has an active or pending trace.
    /// Used by the smali renderer to decide whether to tint the
    /// row.
    pub fn is_traced(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
        method_name: &str,
        method_signature: &str,
    ) -> bool {
        let probe = TraceKey {
            artifact: artifact.clone(),
            class_jni: class_jni.to_string(),
            method_name: method_name.to_string(),
            method_signature: method_signature.to_string(),
        };
        match self.by_key.get(&probe) {
            Some(entry) => matches!(
                entry.status,
                TraceStatus::Pending | TraceStatus::Active
            ),
            None => false,
        }
    }

    /// Whether any method on this class is currently traced.
    /// Faster than walking every method when rendering class
    /// headers in the smali view.
    pub fn class_has_trace(
        &self,
        artifact: &glass_db::ArtifactId,
        class_jni: &str,
    ) -> bool {
        self.by_key.iter().any(|(k, e)| {
            k.artifact == *artifact
                && k.class_jni == class_jni
                && matches!(
                    e.status,
                    TraceStatus::Pending | TraceStatus::Active
                )
        })
    }

    pub fn entries(&self) -> Vec<&TraceEntry> {
        self.by_key.values().collect()
    }

    /// Append an invocation. Drops oldest if over the cap.
    pub fn push_invocation(&mut self, key: &TraceKey, inv: Invocation) {
        let Some(entry) = self.by_key.get_mut(key) else {
            return;
        };
        if entry.invocations.len() >= MAX_INVOCATIONS_PER_TRACE {
            let drop_count =
                entry.invocations.len() - MAX_INVOCATIONS_PER_TRACE + 1;
            entry.invocations.drain(..drop_count);
        }
        entry.invocations.push(inv);
    }

    /// Mark a trace as Active once its script finished loading.
    pub fn mark_active(&mut self, key: &TraceKey, script_id: glass_frida::ScriptId) {
        if let Some(entry) = self.by_key.get_mut(key) {
            entry.script_id = Some(script_id);
            entry.status = TraceStatus::Active;
            self.by_script.insert(script_id, key.clone());
        }
    }

    pub fn mark_failed(&mut self, key: &TraceKey, message: String) {
        if let Some(entry) = self.by_key.get_mut(key) {
            entry.status = TraceStatus::Failed { message };
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

    fn mk_key(method: &str) -> TraceKey {
        TraceKey {
            artifact: glass_db::ArtifactId::from_bytes(b"test"),
            class_jni: "Lcom/example/Foo;".into(),
            method_name: method.into(),
            method_signature: "()V".into(),
        }
    }

    fn mk_entry(key: TraceKey) -> TraceEntry {
        TraceEntry {
            key,
            script_id: None,
            status: TraceStatus::Pending,
            created_at: Instant::now(),
            invocations: Vec::new(),
        }
    }

    #[test]
    fn insert_and_lookup() {
        let mut reg = TraceRegistry::new();
        let key = mk_key("bar");
        reg.insert(mk_entry(key.clone()));
        assert!(reg.is_traced(
            &key.artifact,
            &key.class_jni,
            "bar",
            "()V"
        ));
        assert!(!reg.is_traced(
            &key.artifact,
            &key.class_jni,
            "baz",
            "()V"
        ));
    }

    #[test]
    fn class_has_trace_finds_any_method() {
        let mut reg = TraceRegistry::new();
        let key = mk_key("a");
        let aid = key.artifact.clone();
        let jni = key.class_jni.clone();
        reg.insert(mk_entry(key));
        assert!(reg.class_has_trace(&aid, &jni));
        assert!(!reg.class_has_trace(&aid, "Lother/Class;"));
    }

    #[test]
    fn invocation_buffer_bounded() {
        let mut reg = TraceRegistry::new();
        let key = mk_key("loud");
        reg.insert(mk_entry(key.clone()));
        for i in 0..(MAX_INVOCATIONS_PER_TRACE + 50) {
            reg.push_invocation(
                &key,
                Invocation {
                    at: Instant::now(),
                    kind: InvocationKind::Call,
                    summary: format!("#{i}"),
                },
            );
        }
        let entry = reg.get(&key).unwrap();
        assert_eq!(entry.invocations.len(), MAX_INVOCATIONS_PER_TRACE);
        // Oldest should be #50, newest #1049.
        assert!(entry.invocations.first().unwrap().summary.starts_with("#50"));
        assert!(entry
            .invocations
            .last()
            .unwrap()
            .summary
            .starts_with(&format!("#{}", MAX_INVOCATIONS_PER_TRACE + 49)));
    }

    #[test]
    fn mark_active_updates_reverse_index() {
        let mut reg = TraceRegistry::new();
        let key = mk_key("x");
        reg.insert(mk_entry(key.clone()));
        reg.mark_active(&key, 42);
        assert_eq!(reg.key_for_script(42), Some(&key));
        assert!(matches!(
            reg.get(&key).unwrap().status,
            TraceStatus::Active
        ));
    }
}
