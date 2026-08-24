//! Per-model hardware-compatibility verdicts (SPEC.md §5.2).
//!
//! Scope: pure verdict math only. `ModelState`/`ModelRegistry` (pid, port,
//! status machine) belong to whoever wires up engine.rs; not needed here.

/// Whether a model benefits from running with its size over available RAM.
/// Only MoE/sparse-activation models degrade gracefully over-RAM; dense
/// models stay uniformly slow (SPEC.md §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Moe,
    Dense,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    FitsFast,
    /// MoE running over-RAM but above the measured degradation floor.
    UsableDegraded { tokps_multiplier: f64 },
    NotRecommended,
    /// Permanent, set only after a real failed load attempt (SPEC.md §5.2).
    /// Never produced by `compute_verdict` — callers merge in the cached
    /// `state.json` entry themselves, since it overrides the live ratio.
    UnsupportedArchitecture { error: String },
}

impl Verdict {
    pub fn tier_name(&self) -> &'static str {
        match self {
            Verdict::FitsFast => "FITS_FAST",
            Verdict::UsableDegraded { .. } => "USABLE_DEGRADED",
            Verdict::NotRecommended => "NOT_RECOMMENDED",
            Verdict::UnsupportedArchitecture { .. } => "UNSUPPORTED_ARCHITECTURE",
        }
    }
}

/// MoE degraded-tier interpolation, measured on OLMoE/M4 (PLAN.md):
/// ratio=1.0 -> 1.0x, ratio=0.38 -> 0.514x (7.2 -> 3.7 tok/s).
const MOE_DEGRADED_FLOOR_RATIO: f64 = 0.38;
const MOE_DEGRADED_FLOOR_MULTIPLIER: f64 = 0.514;
const MOE_FULL_MULTIPLIER: f64 = 1.0;

/// Live verdict for one model against current RAM budget (SPEC.md §5.2).
/// `budget_bytes` is `total_ram_bytes - headroom_bytes`, already computed
/// by `residency::resolve_headroom_bytes` + subtraction. Never cached: RAM
/// availability changes between calls.
pub fn compute_verdict(kind: ModelKind, model_size_bytes: u64, budget_bytes: u64) -> Verdict {
    // model_size_bytes == 0 shouldn't happen (manifest always has a real
    // GGUF size), but don't divide by zero if it ever does.
    if model_size_bytes == 0 {
        return Verdict::NotRecommended;
    }
    let ratio = budget_bytes as f64 / model_size_bytes as f64;

    match kind {
        ModelKind::Moe => {
            if ratio >= 1.0 {
                Verdict::FitsFast
            } else if ratio >= MOE_DEGRADED_FLOOR_RATIO {
                let span = 1.0 - MOE_DEGRADED_FLOOR_RATIO;
                let multiplier = MOE_DEGRADED_FLOOR_MULTIPLIER
                    + (MOE_FULL_MULTIPLIER - MOE_DEGRADED_FLOOR_MULTIPLIER)
                        * (ratio - MOE_DEGRADED_FLOOR_RATIO)
                        / span;
                Verdict::UsableDegraded {
                    tokps_multiplier: multiplier,
                }
            } else {
                Verdict::NotRecommended
            }
        }
        ModelKind::Dense => {
            if ratio >= 1.0 {
                Verdict::FitsFast
            } else {
                Verdict::NotRecommended
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn moe_fits_fast_at_ratio_one() {
        let v = compute_verdict(ModelKind::Moe, 10 * GB, 10 * GB);
        assert_eq!(v, Verdict::FitsFast);
        let v = compute_verdict(ModelKind::Moe, 10 * GB, 20 * GB);
        assert_eq!(v, Verdict::FitsFast);
    }

    #[test]
    fn moe_degraded_matches_measured_endpoints() {
        // ratio == 0.38 -> 0.514x (the measured floor point itself).
        let v = compute_verdict(ModelKind::Moe, 100 * GB, 38 * GB);
        match v {
            Verdict::UsableDegraded { tokps_multiplier } => {
                assert!((tokps_multiplier - 0.514).abs() < 1e-9);
            }
            other => panic!("expected UsableDegraded, got {other:?}"),
        }

        // ratio just under 1.0 -> multiplier just under 1.0.
        let v = compute_verdict(ModelKind::Moe, 100 * GB, 99 * GB);
        match v {
            Verdict::UsableDegraded { tokps_multiplier } => {
                assert!(tokps_multiplier > 0.99 && tokps_multiplier < 1.0);
            }
            other => panic!("expected UsableDegraded, got {other:?}"),
        }
    }

    #[test]
    fn moe_not_recommended_below_floor() {
        let v = compute_verdict(ModelKind::Moe, 100 * GB, 37 * GB);
        assert_eq!(v, Verdict::NotRecommended);
    }

    #[test]
    fn dense_has_no_degraded_tier() {
        assert_eq!(
            compute_verdict(ModelKind::Dense, 10 * GB, 10 * GB),
            Verdict::FitsFast
        );
        // Even at ratio 0.99, dense gets no partial-credit tier.
        assert_eq!(
            compute_verdict(ModelKind::Dense, 100 * GB, 99 * GB),
            Verdict::NotRecommended
        );
    }

    #[test]
    fn zero_size_model_does_not_panic() {
        assert_eq!(compute_verdict(ModelKind::Moe, 0, 10 * GB), Verdict::NotRecommended);
    }
}
