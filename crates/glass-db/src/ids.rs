//! Content-addressed identifiers: blake3 of the underlying bytes.
//!
//! Both `BundleId` and `ArtifactId` are 32-byte hashes wrapped in newtypes
//! so they can't be confused at the type level. They serialize as hex
//! strings — readable in the DB and stable across architectures.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BundleId([u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId([u8; 32]);

macro_rules! id_impl {
    ($Ty:ident) => {
        impl $Ty {
            pub fn from_bytes(bytes: &[u8]) -> Self {
                // Use blake3's rayon-backed multi-threaded path for
                // anything past ~128 KiB. Small inputs aren't worth
                // the thread-pool overhead.
                const PARALLEL_THRESHOLD: usize = 128 * 1024;
                let mut hasher = blake3::Hasher::new();
                if bytes.len() >= PARALLEL_THRESHOLD {
                    hasher.update_rayon(bytes);
                } else {
                    hasher.update(bytes);
                }
                let hash = hasher.finalize();
                Self(*hash.as_bytes())
            }

            pub fn from_raw(raw: [u8; 32]) -> Self {
                Self(raw)
            }

            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            pub fn to_hex(&self) -> String {
                let mut s = String::with_capacity(64);
                for b in self.0 {
                    use std::fmt::Write;
                    let _ = write!(s, "{:02x}", b);
                }
                s
            }
        }

        impl std::fmt::Display for $Ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                // Show the first 8 hex chars — enough to eyeball in logs
                // without screaming. Use `to_hex()` for the full thing.
                let h = self.to_hex();
                write!(f, "{}…", &h[..8])
            }
        }
    };
}

id_impl!(BundleId);
id_impl!(ArtifactId);
