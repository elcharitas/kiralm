//! RAM detection and pre-load admission control (SPEC.md §5.1, §5.4).
//!
//! Scope: headroom math and the pre-spawn budget check only. LRU eviction
//! and the RSS background watcher belong to whoever wires up engine.rs.

use sysinfo::System;

const GB: u64 = 1024 * 1024 * 1024;
const HEADROOM_FLOOR_BYTES: u64 = GB; // "Hard floor 1GB" (SPEC.md §3)
const AUTO_HEADROOM_SMALL_RAM_GB: f64 = 2.0;
const AUTO_HEADROOM_DEFAULT_GB: f64 = 1.5;
const SMALL_RAM_THRESHOLD_BYTES: u64 = 8 * GB;

/// Total system RAM in bytes, via `sysinfo` (SPEC.md §5.1).
pub fn total_ram_bytes() -> u64 {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.total_memory()
}

/// Currently available RAM in bytes (`.available_memory()`, not
/// `.free_memory()` — it already accounts for reclaimable OS cache).
pub fn available_ram_bytes() -> u64 {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.available_memory()
}

/// Resolve `ram_headroom_gb` (SPEC.md §3, §5.1): explicit config value if
/// set (rejected below the 1GB hard floor, no override), else auto-tiered
/// by total system RAM.
pub fn resolve_headroom_bytes(total_ram_bytes: u64, configured_gb: Option<f64>) -> Result<u64, String> {
    match configured_gb {
        Some(gb) => {
            let bytes = (gb * GB as f64) as u64;
            if bytes < HEADROOM_FLOOR_BYTES {
                Err(format!(
                    "ram_headroom_gb = {gb} is below the 1GB hard floor; refusing to start"
                ))
            } else {
                Ok(bytes)
            }
        }
        None => {
            let auto_gb = if total_ram_bytes <= SMALL_RAM_THRESHOLD_BYTES {
                AUTO_HEADROOM_SMALL_RAM_GB
            } else {
                AUTO_HEADROOM_DEFAULT_GB
            };
            Ok((auto_gb * GB as f64) as u64)
        }
    }
}

/// Result of a pre-spawn admission check (SPEC.md §5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRejected {
    pub shortfall_bytes: u64,
}

/// Pre-spawn admission check: reject before attempting to load, no spawn
/// attempt made. `already_loaded_bytes` is the sum of `size_bytes` of every
/// currently-loaded (or, for the hot-toggle check, every hot) model; the
/// caller decides which set that is. Never allows loading past the cap.
pub fn check_admission(
    already_loaded_bytes: u64,
    new_model_bytes: u64,
    total_ram_bytes: u64,
    headroom_bytes: u64,
) -> Result<(), AdmissionRejected> {
    let budget = total_ram_bytes.saturating_sub(headroom_bytes);
    let needed = already_loaded_bytes.saturating_add(new_model_bytes);
    if needed <= budget {
        Ok(())
    } else {
        Err(AdmissionRejected {
            shortfall_bytes: needed - budget,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_ram_gets_2gb_default_headroom() {
        assert_eq!(resolve_headroom_bytes(8 * GB, None).unwrap(), 2 * GB);
        assert_eq!(resolve_headroom_bytes(4 * GB, None).unwrap(), 2 * GB);
    }

    #[test]
    fn large_ram_gets_1_5gb_default_headroom() {
        let expected = (1.5 * GB as f64) as u64;
        assert_eq!(resolve_headroom_bytes(16 * GB, None).unwrap(), expected);
    }

    #[test]
    fn configured_headroom_below_floor_is_rejected() {
        assert!(resolve_headroom_bytes(16 * GB, Some(0.5)).is_err());
    }

    #[test]
    fn configured_headroom_at_or_above_floor_is_accepted() {
        assert_eq!(resolve_headroom_bytes(16 * GB, Some(1.0)).unwrap(), GB);
        assert_eq!(resolve_headroom_bytes(16 * GB, Some(3.0)).unwrap(), 3 * GB);
    }

    #[test]
    fn admission_allows_load_that_fits_budget() {
        // 16GB total, 1.5GB headroom -> ~14.5GB budget; 10GB already
        // loaded + 4GB new model fits.
        let headroom = resolve_headroom_bytes(16 * GB, None).unwrap();
        assert!(check_admission(10 * GB, 4 * GB, 16 * GB, headroom).is_ok());
    }

    #[test]
    fn admission_rejects_load_that_exceeds_budget_and_reports_shortfall() {
        let headroom = resolve_headroom_bytes(16 * GB, None).unwrap();
        let err = check_admission(10 * GB, 6 * GB, 16 * GB, headroom).unwrap_err();
        // budget = 16GB - 1.5GB = 14.5GB; needed = 16GB; shortfall = 1.5GB.
        assert_eq!(err.shortfall_bytes, (1.5 * GB as f64) as u64);
    }

    #[test]
    fn admission_exact_fit_is_allowed() {
        let headroom = 2 * GB;
        assert!(check_admission(0, 14 * GB, 16 * GB, headroom).is_ok());
    }
}
