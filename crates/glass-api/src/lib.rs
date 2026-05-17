//! Glass automation API.
//!
//! The capability surface that the CLI (`glass-cli`), the scripting
//! host (`glass-script`), and — eventually — the GUI all share. Each
//! verb in `docs/AutomationAPI.md` resolves to a function in this
//! crate. The CLI's job is `argv → glass_api::* → JSON to stdout`;
//! the GUI's job is `gpui event → glass_api::* → render`.
//!
//! ## Bundle handle
//!
//! Most calls go through a [`Bundle`] handle obtained from
//! [`open`]. The handle owns parsed artifact data and caches the
//! per-query indices the GUI builds at load time (symbol map per
//! artifact, search index, xref maps). Building indices is lazy
//! on first use; subsequent calls reuse the cached version.
//!
//! ## Threading
//!
//! `Bundle` is `Send + Sync` and safe to share across worker
//! threads. The internal index caches are guarded by `RwLock` so
//! parallel queries don't fight; cache fills are serialised.

mod bundle;
mod inspect;

pub use bundle::{open, Bundle, BundleKind};
pub use inspect::{ArtifactInfo, ArtifactKind, BundleInspection};

// Re-export the underlying domain types so consumers depend on
// glass-api only, not the whole crate graph.
pub use glass_db::{ArtifactId, BundleId};
pub use glass_arch_arm64::{Symbol, SymbolKind, SymbolMap, SymbolSources};
