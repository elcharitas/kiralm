import { useEffect, useState } from "react";
import { api, type ModelDetail } from "../api";
import { VerdictBadge } from "../components/VerdictBadge";
import { formatBytes, formatRelativeTime } from "../format";

export function ModelDetailDrawer({ id, onClose }: { id: string; onClose: () => void }) {
  const [detail, setDetail] = useState<ModelDetail | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .model(id)
      .then((d) => !cancelled && setDetail(d))
      .catch((e) => !cancelled && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      cancelled = true;
    };
  }, [id]);

  const manifest = detail?.manifest;
  const state = detail?.state;

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
        {!detail && !error && <div className="drawer-loading">Loading manifest…</div>}

        {detail && (
          <div className="drawer-body">
            {state?.verdict_tier && <VerdictBadge tier={state.verdict_tier} />}
            {state?.verdict_reason && <p className="drawer-verdict-reason">{state.verdict_reason}</p>}

            {!manifest && <p className="settings-hint">No manifest on disk for this model.</p>}

            <dl className="detail-grid">
              {state && (
                <>
                  <dt>Resident</dt>
                  <dd>{state.loaded ? `yes, ${formatBytes(state.rss_bytes)}` : "no"}</dd>

                  <dt>mlock</dt>
                  <dd>{state.hot ? "locked (hot)" : "not locked, toggle Hot to pin"}</dd>

                  <dt>Quant</dt>
                  <dd className="mono">{state.quant}</dd>

                  {state.aliases.length > 0 && (
                    <>
                      <dt>Aliases</dt>
                      <dd className="mono">{state.aliases.join(", ")}</dd>
                    </>
                  )}
                </>
              )}

              {manifest && (
                <>
                  <dt>Architecture</dt>
                  <dd className="mono">{manifest.architecture}</dd>

                  <dt>Mixture-of-Experts</dt>
                  <dd>
                    {manifest.is_moe
                      ? `yes: ${manifest.expert_count?.active}/${manifest.expert_count?.total} active`
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
                </>
              )}
            </dl>
            {/* context-size override has no /api write route yet, add when one lands */}
          </div>
        )}
      </aside>
    </div>
  );
}
