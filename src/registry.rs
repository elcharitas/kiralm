//! In-memory `ModelRegistry`, `ModelState`, name resolution, verdict
//! computation.
//!
//! STUB NOTE: the full `ModelRegistry` (RwLock<HashMap>, load/unload
//! transitions, RSS tracking) is only lightly used by this worktree's
//! `serve`/`pull` wiring. The owning agent for engine/residency should
//! flesh out `ModelState.status` transitions and hook the RSS watcher in.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::cloud::CloudModel;
use crate::state::ModelManifest;

#[derive(Debug, Clone, PartialEq)]
pub enum ModelStatus {
    Unloaded,
    Loading,
    Loaded,
    Failed(String),
}

/// Where a `ModelState` came from. One registry, one resolve path, both
/// kinds flow through the same `/v1/models`/`kiralm list` loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Local,
    Cloud,
}

/// What `ensure_loaded` hands back to a chat-completion call site: a
/// local port to proxy to, or a signal to forward to kiralm-web instead.
/// This is the one place "is this model spawnable locally" is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRoute {
    Local(u16),
    Cloud,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictTier {
    FitsFast,
    UsableDegraded,
    NotRecommended,
    /// Permanent, independent of RAM. Set only after a real
    /// spawn+load attempt fails with a genuine llama.cpp load error.
    UnsupportedArchitecture,
}

impl VerdictTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            VerdictTier::FitsFast => "FITS_FAST",
            VerdictTier::UsableDegraded => "USABLE_DEGRADED",
            VerdictTier::NotRecommended => "NOT_RECOMMENDED",
            VerdictTier::UnsupportedArchitecture => "UNSUPPORTED_ARCHITECTURE",
        }
    }
}

// tok_per_sec_multiplier isn't read yet in this worktree; it's the UI
// detail-drawer's field to surface.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Verdict {
    pub tier: VerdictTier,
    pub reason: String,
    /// Only set for FITS_FAST (1.0) and USABLE_DEGRADED.
    pub tok_per_sec_multiplier: Option<f64>,
}

/// Live ratio-based verdict. Does NOT handle
/// `UNSUPPORTED_ARCHITECTURE`, which is cached separately in state.json and
/// overrides this until a force-retry clears it.
pub fn compute_ratio_verdict(is_moe: bool, model_size_bytes: u64, budget_bytes: u64) -> Verdict {
    let ratio = budget_bytes as f64 / model_size_bytes.max(1) as f64;

    if is_moe {
        if ratio >= 1.0 {
            Verdict {
                tier: VerdictTier::FitsFast,
                reason: "model fits fully in available RAM".to_string(),
                tok_per_sec_multiplier: Some(1.0),
            }
        } else if ratio >= 0.38 {
            let mult = 0.514 + 0.486 * (ratio - 0.38) / 0.62;
            Verdict {
                tier: VerdictTier::UsableDegraded,
                reason: format!(
                    "MoE model runs over-RAM with swap/paging; ratio {ratio:.2} degrades throughput"
                ),
                tok_per_sec_multiplier: Some(mult),
            }
        } else {
            Verdict {
                tier: VerdictTier::NotRecommended,
                reason: "below the measured degradation floor, no verified data past this point"
                    .to_string(),
                tok_per_sec_multiplier: None,
            }
        }
    } else if ratio >= 1.0 {
        Verdict {
            tier: VerdictTier::FitsFast,
            reason: "model fits fully in available RAM".to_string(),
            tok_per_sec_multiplier: Some(1.0),
        }
    } else {
        Verdict {
            tier: VerdictTier::NotRecommended,
            reason: "dense model larger than available RAM runs uniformly slow".to_string(),
            tok_per_sec_multiplier: None,
        }
    }
}

// architecture/is_moe/expert_count/mlocked round-trip the manifest's
// shape but this worktree only reads size_bytes/status/hot/verdict/
// gguf_path so far; the engine flag-selection matrix (engine.rs's
// STUB NOTE) is what consumes the rest.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ModelState {
    pub canonical_id: String,
    pub gguf_path: PathBuf,
    pub architecture: String,
    pub is_moe: bool,
    pub expert_count: Option<(u32, u32)>, // (active, total)
    /// From the GGUF's trained context window (manifest sidecar).
    /// `None` for cloud entries, whose catalog doesn't carry it.
    pub context_length: Option<u64>,
    pub size_bytes: u64,
    pub hot: bool,
    pub status: ModelStatus,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub mlocked: bool,
    pub last_used: Instant,
    /// `None` for `Cloud` entries: no RAM check ever ran (there's no
    /// local file to weigh against headroom).
    pub verdict: Option<Verdict>,
    pub source: ModelSource,
    /// The catalog slug kiralm-web expects in the outgoing `model` field
    /// when forwarding a chat completion (`CloudModel.id`, e.g.
    /// `llama-3.1-70b`; kiralm-web's `/api/v1/chat/completions`
    /// requires the catalog slug, not the internal `ref`). Distinct
    /// from `canonical_id`, which is this daemon's own namespaced id.
    /// `None` for `Local` entries.
    pub cloud_model_id: Option<String>,
}

impl ModelState {
    pub fn from_manifest(m: &ModelManifest, hot: bool) -> ModelState {
        ModelState {
            canonical_id: m.canonical_id.clone(),
            gguf_path: PathBuf::new(), // filled in by caller who knows model_dir
            architecture: m.architecture.clone(),
            is_moe: m.is_moe,
            expert_count: m
                .expert_count
                .as_ref()
                .map(|e| (e.active, e.total)),
            context_length: Some(m.context_length_trained),
            size_bytes: m.size_bytes,
            hot,
            status: ModelStatus::Unloaded,
            pid: None,
            port: None,
            mlocked: false,
            last_used: Instant::now(),
            verdict: Some(Verdict {
                tier: VerdictTier::NotRecommended,
                reason: "not yet evaluated".to_string(),
                tok_per_sec_multiplier: None,
            }),
            source: ModelSource::Local,
            cloud_model_id: None,
        }
    }

    /// Converts one raw kiralm-web catalog record into a registry entry.
    /// Returns `None` for a `kind` this daemon doesn't route chat
    /// completions through yet. Currently `local_pull` rows, which are
    /// curated `kiralm pull` suggestions, not network-invocable models
    /// (kiralm-web's own `/api/v1/models` excludes them too).
    pub fn from_cloud_entry(m: &CloudModel) -> Option<ModelState> {
        // Id namespacing: Workers-AI-hosted models
        // keep kiralm-web's own `@cf/...` ref under a `workers-ai/`
        // prefix; anything else recognized as cloud-routable falls back
        // to `cloud/<catalog-id>` so future kinds don't collide.
        let canonical_id = match m.kind.as_str() {
            "workers_ai" => format!("workers-ai/{}", m.r#ref),
            "local_pull" => return None,
            _ => format!("cloud/{}", m.id),
        };
        Some(ModelState {
            canonical_id,
            gguf_path: PathBuf::new(),
            architecture: String::new(),
            is_moe: false,
            expert_count: None,
            context_length: None,
            size_bytes: m.size_bytes.unwrap_or(0),
            hot: false,
            status: ModelStatus::Loaded,
            pid: None,
            port: None,
            mlocked: false,
            last_used: Instant::now(),
            verdict: None,
            source: ModelSource::Cloud,
            cloud_model_id: Some(m.id.clone()),
        })
    }
}

/// Model count is small (tens); one lock, no database.
#[derive(Default)]
pub struct ModelRegistry {
    inner: RwLock<HashMap<String, ModelState>>,
}

impl ModelRegistry {
    pub fn new() -> ModelRegistry {
        ModelRegistry::default()
    }

    pub fn insert(&self, state: ModelState) {
        self.inner.write().insert(state.canonical_id.clone(), state);
    }

    pub fn get_clone(&self, canonical_id: &str) -> Option<ModelState> {
        self.inner.read().get(canonical_id).cloned()
    }

    pub fn list(&self) -> Vec<ModelState> {
        self.inner.read().values().cloned().collect()
    }

    pub fn remove(&self, canonical_id: &str) {
        self.inner.write().remove(canonical_id);
    }

    /// Wholesale replace of every `Cloud`-sourced entry with a fresh
    /// batch from the latest catalog sync. Local entries are untouched.
    /// A single write lock so readers never see a half-swapped set.
    pub fn replace_cloud_entries(&self, entries: Vec<ModelState>) {
        let mut inner = self.inner.write();
        inner.retain(|_, m| m.source != ModelSource::Cloud);
        for entry in entries {
            inner.insert(entry.canonical_id.clone(), entry);
        }
    }

    /// Resolution order: alias exact match -> canonical id; else
    /// canonical id exact match; else `None` (caller returns 404).
    pub fn resolve(&self, name: &str, aliases: &HashMap<String, String>) -> Option<String> {
        if let Some(canonical) = aliases.get(name) {
            return Some(canonical.clone());
        }
        if self.inner.read().contains_key(name) {
            return Some(name.to_string());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moe_verdict_matches_measured_olmoe_points() {
        // ratio=1.0 -> FITS_FAST, 1.0x (model fits in RAM)
        let v = compute_ratio_verdict(true, 100, 100);
        assert_eq!(v.tier, VerdictTier::FitsFast);
        assert_eq!(v.tok_per_sec_multiplier, Some(1.0));

        // ratio=0.38 -> USABLE_DEGRADED, 0.514x (7.2 -> 3.7 tok/s point)
        let v = compute_ratio_verdict(true, 100, 38);
        assert_eq!(v.tier, VerdictTier::UsableDegraded);
        assert!((v.tok_per_sec_multiplier.unwrap() - 0.514).abs() < 0.01);

        // below the floor -> NOT_RECOMMENDED, no multiplier
        let v = compute_ratio_verdict(true, 100, 20);
        assert_eq!(v.tier, VerdictTier::NotRecommended);
        assert_eq!(v.tok_per_sec_multiplier, None);
    }

    #[test]
    fn dense_verdict_has_no_degraded_tier() {
        assert_eq!(compute_ratio_verdict(false, 100, 100).tier, VerdictTier::FitsFast);
        // even close to fitting, dense over-RAM is NOT_RECOMMENDED, not degraded
        assert_eq!(compute_ratio_verdict(false, 100, 99).tier, VerdictTier::NotRecommended);
    }

    fn cloud_model(kind: &str, id: &str, r#ref: &str) -> CloudModel {
        CloudModel {
            id: id.to_string(),
            name: id.to_string(),
            r#ref: r#ref.to_string(),
            kind: kind.to_string(),
            size_bytes: None,
            description: None,
            enabled: true,
            sort_order: 0,
        }
    }

    #[test]
    fn cloud_entry_namespacing_and_no_ram_story() {
        let m = ModelState::from_cloud_entry(&cloud_model(
            "workers_ai",
            "llama-3.1-70b",
            "@cf/meta/llama-3.1-70b-instruct",
        ))
        .unwrap();
        assert_eq!(m.canonical_id, "workers-ai/@cf/meta/llama-3.1-70b-instruct");
        assert_eq!(m.cloud_model_id.as_deref(), Some("llama-3.1-70b"));
        assert_eq!(m.source, ModelSource::Cloud);
        assert_eq!(m.status, ModelStatus::Loaded);
        assert!(m.verdict.is_none());
        assert_eq!(m.gguf_path, PathBuf::new());
        assert!(m.port.is_none() && m.pid.is_none() && !m.mlocked && !m.hot);
    }

    #[test]
    fn cloud_entry_falls_back_to_cloud_prefix_for_unknown_kind() {
        let m = ModelState::from_cloud_entry(&cloud_model("byok", "my-model", "gpt-4o")).unwrap();
        assert_eq!(m.canonical_id, "cloud/my-model");
    }

    #[test]
    fn local_pull_rows_are_not_cloud_models() {
        assert!(ModelState::from_cloud_entry(&cloud_model("local_pull", "x", "y")).is_none());
    }

    #[test]
    fn replace_cloud_entries_leaves_local_models_alone() {
        let registry = ModelRegistry::new();
        let mut local = ModelState::from_manifest(
            &ModelManifest {
                canonical_id: "local-model".to_string(),
                filename: "x.gguf".to_string(),
                sha256: String::new(),
                size_bytes: 1,
                architecture: "llama".to_string(),
                is_moe: false,
                expert_count: None,
                context_length_trained: 0,
                vision_capable: false,
                source_ref: String::new(),
                pulled_at: chrono::Utc::now(),
            },
            false,
        );
        local.canonical_id = "local-model".to_string();
        registry.insert(local);

        let cloud_entry = ModelState::from_cloud_entry(&cloud_model("workers_ai", "a", "@cf/a")).unwrap();
        registry.replace_cloud_entries(vec![cloud_entry]);
        assert!(registry.get_clone("local-model").is_some());
        assert!(registry.get_clone("workers-ai/@cf/a").is_some());

        // A second sync with a different model wholesale-replaces the
        // first cloud batch but still doesn't touch the local entry.
        let cloud_entry2 = ModelState::from_cloud_entry(&cloud_model("workers_ai", "b", "@cf/b")).unwrap();
        registry.replace_cloud_entries(vec![cloud_entry2]);
        assert!(registry.get_clone("local-model").is_some());
        assert!(registry.get_clone("workers-ai/@cf/a").is_none());
        assert!(registry.get_clone("workers-ai/@cf/b").is_some());
    }
}
