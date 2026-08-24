//! STUB: real registry.rs (§4.2 of SPEC.md) owns model discovery, manifest
//! sidecar parsing, alias resolution and verdict computation. This is a
//! minimal in-memory stand-in so `api/anthropic.rs` has something to resolve
//! model names and vision capability against. Replace wholesale once the
//! real ModelRegistry (backed by state.json / manifest sidecars) lands.

use std::collections::HashMap;
use std::sync::RwLock;

/// Per-model facts the Anthropic route needs. Real registry carries far
/// more (architecture, is_moe, verdict, hot/loaded state, etc. — see
/// SPEC.md §4.2's `ModelState`).
#[derive(Clone, Debug)]
pub struct ModelManifest {
    pub canonical_id: String,
    pub vision_capable: bool,
}

pub struct ModelRegistry {
    // canonical_id/alias -> canonical_id
    names: RwLock<HashMap<String, String>>,
    manifests: RwLock<HashMap<String, ModelManifest>>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self {
            names: RwLock::new(HashMap::new()),
            manifests: RwLock::new(HashMap::new()),
        }
    }

    /// Test/stub helper: real construction happens by loading state.json +
    /// manifest sidecars (§3).
    pub fn register(&self, manifest: ModelManifest, aliases: &[&str]) {
        let canonical = manifest.canonical_id.clone();
        self.names
            .write()
            .unwrap()
            .insert(canonical.clone(), canonical.clone());
        for alias in aliases {
            self.names
                .write()
                .unwrap()
                .insert(alias.to_string(), canonical.clone());
        }
        self.manifests.write().unwrap().insert(canonical, manifest);
    }

    /// §6 resolution order: alias exact match -> canonical id; else
    /// canonical id exact match; else None (caller returns 404).
    pub fn resolve(&self, name: &str) -> Option<String> {
        self.names.read().unwrap().get(name).cloned()
    }

    pub fn manifest(&self, canonical_id: &str) -> Option<ModelManifest> {
        self.manifests.read().unwrap().get(canonical_id).cloned()
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}
