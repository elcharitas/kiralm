//! In-memory `ModelRegistry`, `ModelState`, name resolution, verdict
//! computation (§4.2, §5.2, §6).
//!
//! STUB NOTE: the full `ModelRegistry` (RwLock<HashMap>, load/unload
//! transitions, RSS tracking) is only lightly used by this worktree's
//! `serve`/`pull` wiring. The owning agent for engine/residency should
//! flesh out `ModelState.status` transitions and hook the RSS watcher in.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::state::ModelManifest;

#[derive(Debug, Clone, PartialEq)]
pub enum ModelStatus {
    Unloaded,
    Loading,
    Loaded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictTier {
    FitsFast,
    UsableDegraded,
    NotRecommended,
    /// Permanent, independent of RAM (§5.2). Set only after a real
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
// detail-drawer's field (§9 "verdict detail") to surface.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Verdict {
    pub tier: VerdictTier,
    pub reason: String,
    /// Only set for FITS_FAST (1.0) and USABLE_DEGRADED.
    pub tok_per_sec_multiplier: Option<f64>,
}

/// Live ratio-based verdict per §5.2. Does NOT handle
/// `UNSUPPORTED_ARCHITECTURE` — that's cached separately in state.json and
/// overrides this until a force-retry clears it (§5.2/§5.3).
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

// architecture/is_moe/expert_count/mlocked round-trip §4.2's shape but
// this worktree only reads size_bytes/status/hot/verdict/gguf_path so
// far; the engine flag-selection matrix (engine.rs's STUB NOTE) is what
// consumes the rest.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ModelState {
    pub canonical_id: String,
    pub gguf_path: PathBuf,
    pub architecture: String,
    pub is_moe: bool,
    pub expert_count: Option<(u32, u32)>, // (active, total)
    pub size_bytes: u64,
    pub hot: bool,
    pub status: ModelStatus,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub mlocked: bool,
    pub last_used: Instant,
    pub verdict: Verdict,
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
            size_bytes: m.size_bytes,
            hot,
            status: ModelStatus::Unloaded,
            pid: None,
            port: None,
            mlocked: false,
            last_used: Instant::now(),
            verdict: Verdict {
                tier: VerdictTier::NotRecommended,
                reason: "not yet evaluated".to_string(),
                tok_per_sec_multiplier: None,
            },
        }
    }
}

/// Model count is small (tens); one lock, no database (§4.2).
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

    /// Resolution order (§6): alias exact match -> canonical id; else
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
}
