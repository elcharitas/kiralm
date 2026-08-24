import { useEffect, useState } from "react";
import { api, type ModelManifest } from "../api";
import { VerdictBadge } from "../components/VerdictBadge";
import { formatBytes, formatRelativeTime } from "../format";

export function ModelDetailDrawer({ id, onClose }: { id: string; onClose: () => void }) {
  const [manifest, setManifest] = useState<ModelManifest | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .model(id)
      .then((m) => !cancelled && setManifest(m))
      .catch((e) => !cancelled && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      cancelled = true;
    };
  }, [id]);

  return (
    <div className="drawer-overlay" onClick={onClose}>
      <aside className="drawer" onClick={(e) => e.stopPropagation()}>
        <div className="drawer-header">
          <h2 className="mono">{id}</h2>
          <button type="button" className="btn btn-ghost" onClick={onClose}>
            Close
          </button>
        </div>

        {error && <div className="banner banner-bad">{error}</div>}
        {!manifest && !error && <div className="drawer-loading">Loading manifest…</div>}

        {manifest && (
          <div className="drawer-body">
            <VerdictBadge tier={manifest.verdict_tier} />
            <p className="drawer-verdict-reason">{manifest.verdict_reason}</p>

            <dl className="detail-grid">
              <dt>Architecture</dt>
              <dd className="mono">{manifest.architecture}</dd>

              <dt>Mixture-of-Experts</dt>
              <dd>
                {manifest.is_moe
                  ? `yes — ${manifest.expert_count?.active}/${manifest.expert_count?.total} active`
                  : "no (dense)"}
              </dd>

              <dt>Size on disk</dt>
              <dd className="mono">{formatBytes(manifest.size_bytes)}</dd>

              <dt>Trained context</dt>
              <dd className="mono">{manifest.context_length_trained.toLocaleString()} tokens</dd>

              <dt>Vision</dt>
              <dd>{manifest.vision_capable ? "yes" : "no"}</dd>

              <dt>Source</dt>
              <dd className="mono">{manifest.source_ref}</dd>

              <dt>SHA-256</dt>
              <dd className="mono truncate" title={manifest.sha256}>
                {manifest.sha256}
              </dd>

              <dt>Pulled</dt>
              <dd>{formatRelativeTime(manifest.pulled_at)}</dd>

              <dt>Last used</dt>
              <dd>{formatRelativeTime(manifest.last_used)}</dd>

              <dt>mlock</dt>
              <dd>{manifest.hot ? "locked (hot)" : "not locked — toggle Hot to pin"}</dd>
            </dl>
            {/* context-size override has no /api write route yet in SPEC — add when one lands */}
          </div>
        )}
      </aside>
    </div>
  );
}
