//! In-memory registry of staged edits.
//!
//! Three kinds of edit share the same registry:
//! - `Instruction` — one AArch64 word (4 bytes), staged from
//!   double-clicking a disasm row. `new_text` carries the typed
//!   source; `display` caches the pretty-printed disasm of the
//!   new bytes.
//! - `Bytes` — a single byte (or short run) edited in the hex
//!   view's byte column. `display` is the hex pair.
//! - `String` — a NUL-padded C-string in a data section (e.g.
//!   `__cstring`). `new_text` is the literal string; the bytes
//!   are NUL-padded to the original length.
//!
//! All three are keyed by `(artifact, vaddr)`. Length-changing
//! edits would force us to re-flow the section + every later
//! edit's address, so for v1 every edit is a **fixed-length
//! splice**: `new_bytes.len() == original_bytes.len()`. Strings
//! get NUL-padded to honour this.
//!
//! Edits are in-memory only — closing the bundle drops them.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Instruction,
    Bytes,
    String,
}

#[derive(Debug, Clone)]
pub struct Edit {
    pub artifact: glass_db::ArtifactId,
    /// Virtual address where the splice begins.
    pub vaddr: u64,
    pub kind: EditKind,
    /// New bytes to splice in. For same-size and shrink edits,
    /// `len() == original_bytes.len()`. For 2-byte → 4-byte
    /// Thumb edits that absorb a following NOP, `len()` is 4
    /// while `original_bytes.len()` is 2 and
    /// `absorbed_following` records how many extra original
    /// bytes (here: 2) were consumed.
    pub new_bytes: Vec<u8>,
    /// Bytes that were there before. Kept so the Changes dialog
    /// can render "was X, now Y" and Revert can roll back without
    /// touching disk.
    pub original_bytes: Vec<u8>,
    /// What the user typed (raw source — assembly for Instruction,
    /// the hex pair for Bytes, the text contents for String).
    pub source_text: String,
    /// Cached display string. For Instruction: the pretty disasm
    /// of `new_bytes`. For Bytes: the hex (`"42"`). For String:
    /// the new text (matching `source_text`).
    pub display: String,
    /// Number of bytes from the original instruction stream this
    /// edit consumed *beyond* its own original slot. Used for
    /// nop-absorbing grow edits: a 2-byte Thumb-1 instruction
    /// grown to 4 bytes by consuming an adjacent Thumb-1 NOP
    /// at `vaddr + 2` records `absorbed_following = 2`. The
    /// listing renderer hides instruction rows whose addresses
    /// fall inside the absorbed range, so the user sees a
    /// single 4-byte row in place of the original 2 + 2.
    /// 0 for the typical same-width and shrink cases.
    pub absorbed_following: u8,
}

impl Edit {
    pub fn len(&self) -> usize {
        self.new_bytes.len()
    }
}

#[derive(Default, Debug, Clone)]
pub struct EditRegistry {
    /// Keyed by (artifact, vaddr). HashMap because ArtifactId
    /// isn't Ord; we sort on iteration for the dialog.
    by_key: HashMap<(glass_db::ArtifactId, u64), Edit>,
}

impl EditRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn insert(&mut self, edit: Edit) {
        self.by_key
            .insert((edit.artifact.clone(), edit.vaddr), edit);
    }

    pub fn get(&self, artifact: &glass_db::ArtifactId, vaddr: u64) -> Option<&Edit> {
        self.by_key.get(&(artifact.clone(), vaddr))
    }

    /// Find any edit whose `[vaddr, vaddr+len)` range contains
    /// `query`. Used by the hex view to decide whether a given
    /// byte address is part of a longer staged string edit.
    pub fn covering(
        &self,
        artifact: &glass_db::ArtifactId,
        query: u64,
    ) -> Option<&Edit> {
        for ((aid, start), edit) in &self.by_key {
            if aid != artifact {
                continue;
            }
            let end = start.saturating_add(edit.new_bytes.len() as u64);
            if query >= *start && query < end {
                return Some(edit);
            }
        }
        None
    }

    pub fn remove(&mut self, artifact: &glass_db::ArtifactId, vaddr: u64) -> Option<Edit> {
        self.by_key.remove(&(artifact.clone(), vaddr))
    }

    pub fn clear(&mut self) {
        self.by_key.clear();
    }

    /// Stable iteration in (artifact, vaddr) order. We sort
    /// at call time — there are typically only a handful of
    /// edits, so the cost is negligible and we avoid the
    /// Ord-on-ArtifactId requirement.
    pub fn entries(&self) -> Vec<&Edit> {
        let mut out: Vec<&Edit> = self.by_key.values().collect();
        out.sort_by_key(|e| (e.artifact.to_string(), e.vaddr));
        out
    }
}
