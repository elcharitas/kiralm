//! STUB. SPEC.md §4 describes `engine.rs` as owning llama-server child
//! spawn/health-check/kill and §5 describes admission control living in
//! `residency.rs`. Neither exists in this worktree yet (this task only
//! covers the OpenAI HTTP surface), so `EngineSupervisor` here only
//! implements the interface the API routes need: name resolution (§6) and
//! a `Loaded -> port` lookup. It does not spawn anything — a model stuck
//! `Unloaded`/`Loading` is reported as still cold-starting (503), which is
//! a real, spec-correct response, just never resolved by an actual spawn.
//! Swap this module's body for the real supervisor without touching
//! api/openai.rs; the public interface is designed to stay stable.

use std::collections::HashMap;

use parking_lot::RwLock;

use crate::registry::{ModelState, ModelStatus};

pub struct EngineSupervisor {
    models: RwLock<HashMap<String, ModelState>>,
    aliases: RwLock<HashMap<String, String>>,
}

/// Failure modes `ensure_loaded` can report; maps 1:1 to SPEC §2.4.
pub enum EnsureLoadError {
    /// Cached-unsupported-architecture (§5.2/§5.3): permanent, 4xx.
    Unsupported(String),
    /// Child not yet healthy (still `Unloaded`/`Loading`/`Failed`): 503.
    ColdStarting,
}

impl EngineSupervisor {
    pub fn new() -> Self {
        Self {
            models: RwLock::new(HashMap::new()),
            aliases: RwLock::new(HashMap::new()),
        }
    }

    /// Test/dev seeding hook — the real supervisor populates this from
    /// state.json + manifest sidecars (§3) at daemon startup instead.
    pub fn insert(&self, model: ModelState) {
        self.models
            .write()
            .insert(model.canonical_id.clone(), model);
    }

    pub fn insert_alias(&self, alias: &str, canonical_id: &str) {
        self.aliases
            .write()
            .insert(alias.to_string(), canonical_id.to_string());
    }

    /// SPEC §6 resolution order: alias exact match -> canonical id exact
    /// match -> None (caller returns 404/model_not_found).
    pub fn resolve(&self, name: &str) -> Option<String> {
        if let Some(canonical) = self.aliases.read().get(name) {
            return Some(canonical.clone());
        }
        if self.models.read().contains_key(name) {
            return Some(name.to_string());
        }
        None
    }

    /// Ensures a child is loaded and returns its port. Does not spawn
    /// (see module doc) — only reports the current state.
    pub fn ensure_loaded(&self, canonical_id: &str) -> Result<u16, EnsureLoadError> {
        let models = self.models.read();
        let model = models
            .get(canonical_id)
            .ok_or(EnsureLoadError::ColdStarting)?;
        match &model.status {
            ModelStatus::Loaded => model.port.ok_or(EnsureLoadError::ColdStarting),
            ModelStatus::UnsupportedArchitecture(err) => {
                Err(EnsureLoadError::Unsupported(err.clone()))
            }
            ModelStatus::Unloaded | ModelStatus::Loading | ModelStatus::Failed(_) => {
                Err(EnsureLoadError::ColdStarting)
            }
        }
    }

    /// Every downloaded model regardless of loaded state (SPEC §2.1).
    pub fn list_models(&self) -> Vec<ModelState> {
        self.models.read().values().cloned().collect()
    }
}

impl Default for EngineSupervisor {
    fn default() -> Self {
        Self::new()
    }
}
