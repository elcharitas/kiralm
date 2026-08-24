import type { VerdictTier } from "../api";

const TIER_LABEL: Record<VerdictTier, string> = {
  FITS_FAST: "Fits fast",
  USABLE_DEGRADED: "Usable (degraded)",
  NOT_RECOMMENDED: "Not recommended",
  UNSUPPORTED_ARCHITECTURE: "Unsupported",
};

const TIER_CLASS: Record<VerdictTier, string> = {
  FITS_FAST: "badge-good",
  USABLE_DEGRADED: "badge-warn",
  NOT_RECOMMENDED: "badge-bad",
  UNSUPPORTED_ARCHITECTURE: "badge-bad",
};

export function VerdictBadge({ tier, title }: { tier: VerdictTier; title?: string }) {
  return (
    <span className={`badge ${TIER_CLASS[tier]}`} title={title}>
      {TIER_LABEL[tier]}
    </span>
  );
}
