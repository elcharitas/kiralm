//! Admission control, headroom math, RSS watcher.
//!
//! STUB NOTE: LRU eviction and the 5s-tick background RSS watcher are
//! not implemented here. This worktree only needed the pre-spawn
//! admission check for the pull load-verification pass and for `serve`
//! to wire something real at startup. Whichever agent owns the
//! full residency manager should replace `AdmissionState` with the real
//! `ModelRegistry`-driven accounting and add the watcher task.

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
