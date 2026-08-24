//! `~/.kiralm/state.json` (daemon-owned, atomic rewrite) and the
//! `<file>.gguf.json` manifest sidecars (§3).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::kiralm_home;

pub fn state_path() -> PathBuf {
    kiralm_home().join("state.json")
}

/// One GGUF's manifest sidecar, written once at pull time, immutable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManifest {
    pub canonical_id: String,
    pub filename: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub architecture: String,
    pub is_moe: bool,
    pub expert_count: Option<ExpertCount>,
    pub context_length_trained: u64,
    pub vision_capable: bool,
    pub source_ref: String,
    pub pulled_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpertCount {
    pub total: u32,
    pub active: u32,
}

impl ModelManifest {
    /// Sidecar path for a given GGUF file path: `<file>.gguf.json`.
    pub fn sidecar_path_for(gguf_path: &Path) -> PathBuf {
        let mut s = gguf_path.as_os_str().to_owned();
        s.push(".json");
        PathBuf::from(s)
    }

    pub fn load(gguf_path: &Path) -> anyhow::Result<ModelManifest> {
        let raw = std::fs::read_to_string(Self::sidecar_path_for(gguf_path))?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Manifest sidecars are immutable once written (§3). Write is only
    /// ever called once, at pull time.
    pub fn write(&self, gguf_path: &Path) -> anyhow::Result<()> {
        let path = Self::sidecar_path_for(gguf_path);
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(path, body)?;
        Ok(())
    }

    /// Loads every manifest sidecar found in `model_dir`.
    pub fn load_all(model_dir: &Path) -> anyhow::Result<Vec<ModelManifest>> {
        let mut out = Vec::new();
        if !model_dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(model_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(raw) = std::fs::read_to_string(&path) {
                    if let Ok(m) = serde_json::from_str::<ModelManifest>(&raw) {
                        out.push(m);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictCache {
    pub engine_version: String,
    /// "supported" | "unsupported"
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelEntry {
    pub hot: bool,
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
    pub verdict_cache: Option<VerdictCache>,
}

/// Daemon-owned tunnel runtime state (§"Tunnel client (daemon)"). The
/// raw Cloudflare token lives separately in `tunnel_token.json` (a
/// secret, not status); this is just enough to report `kiralm tunnel
/// status` and know whether to re-spawn `cloudflared` on daemon restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelState {
    pub enabled: bool,
    pub hostname: String,
    pub cf_tunnel_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    pub models: HashMap<String, ModelEntry>,
    pub aliases: HashMap<String, String>,
    #[serde(default)]
    pub tunnel: Option<TunnelState>,
}

impl State {
    pub fn load() -> anyhow::Result<State> {
        Self::load_from(&state_path())
    }

    pub fn load_from(path: &Path) -> anyhow::Result<State> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic rewrite: write to a tmp file in the same dir, then rename
    /// over the target so a crash mid-write never leaves a torn file.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&state_path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}
