//! Admission control, headroom math, RSS watcher (§5).
//!
//! STUB NOTE: LRU eviction and the 5s-tick background RSS watcher (§5.4
//! "Ongoing") are not implemented here — this worktree only needed the
//! pre-spawn admission check for the pull load-verification pass (§8) and
//! for `serve` to wire something real at startup. Whichever agent owns the
//! full residency manager should replace `AdmissionState` with the real
//! `ModelRegistry`-driven accounting and add the watcher task.

use sysinfo::System;

#[derive(Debug, Clone, Copy)]
pub struct RamInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// Reads live RAM info. `.available_memory()`, not `.free_memory()` — it
/// already accounts for reclaimable OS cache (§5.1).
pub fn read_ram_info() -> RamInfo {
    let mut sys = System::new();
    sys.refresh_memory();
    RamInfo {
        total_bytes: sys.total_memory(),
        available_bytes: sys.available_memory(),
    }
}

/// `ram_headroom_gb` from config if set, else auto-tiered: 2GB if total
/// RAM <= 8GB, else 1.5GB (§5.1). Config-level `< 1GB` rejection already
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

/// Pre-spawn admission check (§5.4): reject unless
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
}
