//! Admission control, headroom math, and eviction policy.

use std::time::Instant;
use sysinfo::System;

#[derive(Debug, Clone, Copy)]
pub struct RamInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// Reads live RAM info. `.available_memory()`, not `.free_memory()`, since it
/// already accounts for reclaimable OS cache.
// sysinfo 0.32 reliably returns 0 for available_memory() on
// some macOS setups (reproduced here) -- fall back to free_memory(),
// which undercounts reclaimable pages but is never wrong-in-a-scary-way.
// This is display-only (see admission math in check_admission, which
// never reads available_bytes); upgrade path is bumping sysinfo once
// upstream fixes it.
pub fn read_ram_info() -> RamInfo {
    let mut sys = System::new();
    sys.refresh_memory();
    let available = sys.available_memory();
    RamInfo {
        total_bytes: sys.total_memory(),
        available_bytes: if available > 0 { available } else { sys.free_memory() },
    }
}

/// `ram_headroom_gb` from config if set, else auto-tiered: 2GB if total
/// RAM <= 8GB, else 1.5GB. Config-level `< 1GB` rejection already
/// happens in `Config::load`; this just does the GB->bytes conversion.
pub fn headroom_bytes(total_ram_bytes: u64, configured_gb: Option<f64>) -> u64 {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let gb = configured_gb.unwrap_or(if total_ram_bytes <= 8 * (GB as u64) {
        2.0
    } else {
        1.5
    });
    (gb * GB) as u64
}

/// Reads a loaded child `llama-server`'s real resident memory. `None`
/// if the pid is gone (already exited).
pub fn read_rss_bytes(pid: u32) -> Option<u64> {
    let mut sys = System::new();
    let pid = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).map(|p| p.memory())
}

/// Pre-spawn admission check: reject unless
/// `sum(loaded) + new <= total - headroom`. Returns the shortfall in
/// bytes as `Err` when it doesn't fit.
pub fn check_admission(
    currently_loaded_bytes: u64,
    new_model_bytes: u64,
    total_ram_bytes: u64,
    headroom_bytes: u64,
) -> Result<(), u64> {
    let budget = total_ram_bytes.saturating_sub(headroom_bytes);
    let needed = currently_loaded_bytes + new_model_bytes;
    if needed <= budget {
        Ok(())
    } else {
        Err(needed - budget)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidencyCandidate {
    pub canonical_id: String,
    pub hot: bool,
    pub active_requests: usize,
    pub ready: bool,
    pub last_used: Instant,
}

/// Returns cold, idle, ready models from least to most recent use.
pub fn lru_eviction_order(
    candidates: impl IntoIterator<Item = ResidencyCandidate>,
) -> Vec<ResidencyCandidate> {
    let mut eligible: Vec<_> = candidates
        .into_iter()
        .filter(|item| item.ready && !item.hot && item.active_requests == 0)
        .collect();
    eligible.sort_by(|left, right| {
        left.last_used
            .cmp(&right.last_used)
            .then_with(|| left.canonical_id.cmp(&right.canonical_id))
    });
    eligible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headroom_auto_tiers() {
        let gb = 1024u64 * 1024 * 1024;
        assert_eq!(headroom_bytes(8 * gb, None), 2 * gb);
        assert_eq!(headroom_bytes(16 * gb, None), (1.5 * gb as f64) as u64);
    }

    #[test]
    fn headroom_configured_overrides() {
        let gb = 1024u64 * 1024 * 1024;
        assert_eq!(headroom_bytes(64 * gb, Some(3.0)), 3 * gb);
    }

    #[test]
    fn admission_rejects_over_budget() {
        let gb = 1024u64 * 1024 * 1024;
        let total = 8 * gb;
        let headroom = headroom_bytes(total, None);
        assert!(check_admission(0, 5 * gb, total, headroom).is_ok());
        assert!(check_admission(0, 7 * gb, total, headroom).is_err());
    }

    #[test]
    fn lru_excludes_hot_active_and_non_ready_models() {
        let now = Instant::now();
        let candidate = |id: &str, age: u64, hot, active_requests, ready| ResidencyCandidate {
            canonical_id: id.to_string(),
            hot,
            active_requests,
            ready,
            last_used: now - std::time::Duration::from_secs(age),
        };

        let order = lru_eviction_order([
            candidate("recent", 10, false, 0, true),
            candidate("oldest", 30, false, 0, true),
            candidate("hot", 40, true, 0, true),
            candidate("active", 50, false, 1, true),
            candidate("loading", 60, false, 0, false),
        ]);

        let ids: Vec<_> = order
            .iter()
            .map(|item| item.canonical_id.as_str())
            .collect();
        assert_eq!(ids, ["oldest", "recent"]);
    }
}
