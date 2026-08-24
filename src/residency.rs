//! STUB: real residency.rs (§5 of SPEC.md) owns admission control, LRU
//! eviction, headroom math, and spawning/health-checking `llama-server`
//! children via engine.rs. This stand-in just maps a canonical model id to
//! a "child port" so `api/anthropic.rs` can be built and tested against a
//! fake upstream. Replace wholesale once the real residency manager lands.

use std::collections::HashMap;
use std::sync::RwLock;

/// Mirrors the outcomes the real residency manager can hand back to a
/// route handler (§2.4 error-shape table): not_found (unresolved model,
/// shouldn't normally reach here since routes resolve via registry first),
/// cold-starting (still spawning, past the caller's patience),
/// unsupported-architecture (permanent, cached real error text), or an
/// unexpected proxy failure.
#[derive(Debug)]
pub enum EnsureLoadedError {
    NotFound,
    ColdStarting,
    Unsupported(String),
    Other(String),
}

pub struct Residency {
    // canonical_id -> port of an already-"loaded" child, for stub/testing.
    loaded: RwLock<HashMap<String, u16>>,
}

impl Residency {
    pub fn new() -> Self {
        Self {
            loaded: RwLock::new(HashMap::new()),
        }
    }

    /// Test/stub helper: real path spawns llama-server through engine.rs
    /// under admission control (§5.4) and polls /health (§4.4).
    pub fn set_loaded(&self, canonical_id: &str, port: u16) {
        self.loaded
            .write()
            .unwrap()
            .insert(canonical_id.to_string(), port);
    }

    /// Ensures `canonical_id`'s child is loaded and returns its port.
    pub async fn ensure_loaded(&self, canonical_id: &str) -> Result<u16, EnsureLoadedError> {
        self.loaded
            .read()
            .unwrap()
            .get(canonical_id)
            .copied()
            .ok_or(EnsureLoadedError::NotFound)
    }
}

impl Default for Residency {
    fn default() -> Self {
        Self::new()
    }
}
