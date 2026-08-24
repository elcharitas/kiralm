//! `state.json` (daemon-owned, atomic rewrite) and the per-GGUF manifest
//! sidecar. SPEC.md §3.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictCacheStatus {
    Supported,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictCache {
    pub engine_version: String,
    pub status: VerdictCacheStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedModelState {
    pub hot: bool,
    pub last_used: DateTime<Utc>,
    #[serde(default)]
    pub verdict_cache: Option<VerdictCache>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub models: HashMap<String, PersistedModelState>,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
}

impl State {
    /// Missing file = fresh empty state (first daemon start).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(State::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading state at {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing state at {}", path.display()))
    }

    /// Atomic rewrite: write to a sibling tmp file, then rename over the
    /// target so a crash mid-write never leaves a truncated state.json.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp_path, body)
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("renaming {} -> {}", tmp_path.display(), path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpertCount {
    pub total: u32,
    pub active: u32,
}

/// `<file>.gguf.json` sidecar, written once at pull time, immutable after.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub canonical_id: String,
    pub filename: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub architecture: String,
    pub is_moe: bool,
    pub expert_count: Option<ExpertCount>,
    pub context_length_trained: u32,
    pub vision_capable: bool,
    pub source_ref: String,
    pub pulled_at: DateTime<Utc>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest at {}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing manifest at {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
    }
}
