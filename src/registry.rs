//! In-memory `ModelRegistry`: SPEC.md §4.2, plus verdict computation §5.2
//! and name resolution §6.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use parking_lot::RwLock;

pub type CanonicalId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    Unloaded,
    Loading,
    Loaded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    FitsFast,
    UsableDegraded { tokens_per_sec_multiplier: f64 },
    NotRecommended,
    /// Permanent, independent of RAM. Holds the real llama.cpp error text.
    UnsupportedArchitecture(String),
}

/// SPEC.md §5.2. `ratio = usable_ram / on-disk GGUF size`, computed live,
/// never cached (RAM availability changes). Architecture-unsupported is
/// layered on top by the caller from the state.json verdict_cache — this
/// function only ever produces a RAM-based verdict.
pub fn compute_ram_verdict(size_bytes: u64, is_moe: bool, total_ram_bytes: u64, headroom_bytes: u64) -> Verdict {
    let usable = total_ram_bytes.saturating_sub(headroom_bytes) as f64;
    let ratio = if size_bytes == 0 { 0.0 } else { usable / size_bytes as f64 };

    if is_moe {
        if ratio >= 1.0 {
            Verdict::FitsFast
        } else if ratio >= 0.38 {
            let multiplier = 0.514 + 0.486 * (ratio - 0.38) / 0.62;
            Verdict::UsableDegraded { tokens_per_sec_multiplier: multiplier }
        } else {
            Verdict::NotRecommended
        }
    } else if ratio >= 1.0 {
        Verdict::FitsFast
    } else {
        Verdict::NotRecommended
    }
}

#[derive(Debug, Clone)]
pub struct ModelState {
    pub canonical_id: CanonicalId,
    pub gguf_path: PathBuf,
    pub architecture: String,
    pub is_moe: bool,
    pub expert_count: Option<(u32, u32)>, // (active, total)
    pub size_bytes: u64,
    pub context_length_trained: u32,
    pub hot: bool,
    pub status: ModelStatus,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub mlocked: bool,
    pub last_used: Instant,
    pub verdict: Verdict,
}

impl ModelState {
    pub fn is_loaded(&self) -> bool {
        matches!(self.status, ModelStatus::Loaded)
    }
}

/// Model count is small (tens); one lock, no database. SPEC.md §4.2.
pub struct ModelRegistry {
    inner: RwLock<HashMap<CanonicalId, ModelState>>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        ModelRegistry { inner: RwLock::new(HashMap::new()) }
    }

    pub fn insert(&self, state: ModelState) {
        self.inner.write().insert(state.canonical_id.clone(), state);
    }

    pub fn get(&self, id: &str) -> Option<ModelState> {
        self.inner.read().get(id).cloned()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.inner.read().contains_key(id)
    }

    pub fn all_ids(&self) -> Vec<CanonicalId> {
        self.inner.read().keys().cloned().collect()
    }

    pub fn set_status(&self, id: &str, status: ModelStatus) {
        if let Some(m) = self.inner.write().get_mut(id) {
            m.status = status;
        }
    }

    pub fn mark_loaded(&self, id: &str, pid: u32, port: u16, mlocked: bool) {
        if let Some(m) = self.inner.write().get_mut(id) {
            m.status = ModelStatus::Loaded;
            m.pid = Some(pid);
            m.port = Some(port);
            m.mlocked = mlocked;
            m.last_used = Instant::now();
        }
    }

    pub fn mark_unloaded(&self, id: &str) {
        if let Some(m) = self.inner.write().get_mut(id) {
            m.status = ModelStatus::Unloaded;
            m.pid = None;
            m.port = None;
            m.mlocked = false;
        }
    }

    pub fn set_hot(&self, id: &str, hot: bool) {
        if let Some(m) = self.inner.write().get_mut(id) {
            m.hot = hot;
        }
    }

    pub fn touch_last_used(&self, id: &str) {
        if let Some(m) = self.inner.write().get_mut(id) {
            m.last_used = Instant::now();
        }
    }

    pub fn set_verdict(&self, id: &str, verdict: Verdict) {
        if let Some(m) = self.inner.write().get_mut(id) {
            m.verdict = verdict;
        }
    }

    /// Sum of `size_bytes` for currently-`Loaded` models, optionally
    /// excluding one id (used when re-checking admission for that id).
    pub fn loaded_size_sum(&self, exclude: Option<&str>) -> u64 {
        self.inner
            .read()
            .values()
            .filter(|m| m.is_loaded() && Some(m.canonical_id.as_str()) != exclude)
            .map(|m| m.size_bytes)
            .sum()
    }

    /// Sum of `size_bytes` for models flagged `hot`, regardless of load
    /// state (hot-toggle admission checks the declared budget, not RSS).
    pub fn hot_size_sum(&self, exclude: Option<&str>) -> u64 {
        self.inner
            .read()
            .values()
            .filter(|m| m.hot && Some(m.canonical_id.as_str()) != exclude)
            .map(|m| m.size_bytes)
            .sum()
    }

    /// Loaded, non-hot models ordered oldest-`last_used`-first: strict LRU
    /// eviction order for the residency watcher. SPEC.md §5.4.
    pub fn cold_loaded_by_lru(&self) -> Vec<CanonicalId> {
        let guard = self.inner.read();
        let mut cold: Vec<&ModelState> = guard.values().filter(|m| m.is_loaded() && !m.hot).collect();
        cold.sort_by_key(|m| m.last_used);
        cold.into_iter().map(|m| m.canonical_id.clone()).collect()
    }

    pub fn loaded_pids(&self) -> Vec<(CanonicalId, u32)> {
        self.inner
            .read()
            .values()
            .filter(|m| m.is_loaded())
            .filter_map(|m| m.pid.map(|pid| (m.canonical_id.clone(), pid)))
            .collect()
    }

    /// Alias exact match -> canonical id; else canonical id exact match;
    /// else `None`. No fuzzy matching. SPEC.md §6.
    pub fn resolve(&self, name: &str, aliases: &HashMap<String, String>) -> Option<CanonicalId> {
        if let Some(canonical) = aliases.get(name) {
            return Some(canonical.clone());
        }
        if self.contains(name) {
            return Some(name.to_string());
        }
        None
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::thread::sleep;
    use std::time::Duration as StdDuration;

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn moe_verdict_tiers_match_spec_measured_points() {
        // ratio == 1.0 -> FITS_FAST
        assert_eq!(compute_ram_verdict(10 * GB, true, 10 * GB, 0), Verdict::FitsFast);
        // ratio == 0.38 -> USABLE_DEGRADED at the measured 0.514x floor
        let v = compute_ram_verdict(100 * GB, true, 38 * GB, 0);
        match v {
            Verdict::UsableDegraded { tokens_per_sec_multiplier } => {
                assert!((tokens_per_sec_multiplier - 0.514).abs() < 1e-9);
            }
            other => panic!("expected UsableDegraded, got {other:?}"),
        }
        // just under the floor -> NOT_RECOMMENDED
        assert_eq!(compute_ram_verdict(100 * GB, true, 37 * GB, 0), Verdict::NotRecommended);
    }

    #[test]
    fn dense_models_have_no_degraded_tier() {
        assert_eq!(compute_ram_verdict(10 * GB, false, 10 * GB, 0), Verdict::FitsFast);
        assert_eq!(compute_ram_verdict(10 * GB, false, 9 * GB, 0), Verdict::NotRecommended);
    }

    fn dummy_model(id: &str, hot: bool) -> ModelState {
        ModelState {
            canonical_id: id.to_string(),
            gguf_path: PathBuf::from("/tmp/x.gguf"),
            architecture: "llama".to_string(),
            is_moe: false,
            expert_count: None,
            size_bytes: GB,
            context_length_trained: 8192,
            hot,
            status: ModelStatus::Loaded,
            pid: Some(1),
            port: Some(8700),
            mlocked: false,
            last_used: Instant::now(),
            verdict: Verdict::FitsFast,
        }
    }

    #[test]
    fn cold_lru_eviction_order_is_oldest_first_and_skips_hot() {
        let reg = ModelRegistry::new();
        reg.insert(dummy_model("a", false));
        sleep(StdDuration::from_millis(5));
        reg.insert(dummy_model("b", true)); // hot, must never appear
        sleep(StdDuration::from_millis(5));
        reg.insert(dummy_model("c", false));

        let order = reg.cold_loaded_by_lru();
        assert_eq!(order, vec!["a".to_string(), "c".to_string()]);
    }
}
