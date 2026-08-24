import { useEffect, useState } from "react";
import { api, type ModelStatus, type StatusResponse } from "../api";
import { VerdictBadge } from "../components/VerdictBadge";
import { HotToggle } from "../components/HotToggle";
import { ProgressBar } from "../components/ProgressBar";
import { formatBytes } from "../format";
import { usePullProgress } from "../hooks/usePullProgress";
import { ModelDetailDrawer } from "./ModelDetailDrawer";

export function ModelsView({
  status,
  refresh,
}: {
  status: StatusResponse | null;
  refresh: () => void;
}) {
  const [pullRef, setPullRef] = useState("");
  const [pulling, setPulling] = useState<{ id: string; error: string | null } | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const submitPull = async (e: React.FormEvent) => {
    e.preventDefault();
    const ref = pullRef.trim();
    if (!ref) return;
    try {
      const { id } = await api.pull(ref);
      setPulling({ id, error: null });
      setPullRef("");
    } catch (err) {
      setPulling({ id: ref, error: err instanceof Error ? err.message : String(err) });
    }
  };

  const toggleHot = async (m: ModelStatus, force = false) => {
    setBusy(m.id);
    try {
      await api.setHot(m.id, !m.hot, force);
      refresh();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      if (!force && !m.hot && confirm(`${message}\n\nForce load anyway?`)) {
        await toggleHot(m, true);
        return;
      }
      alert(message);
    } finally {
      setBusy(null);
    }
  };

  const remove = async (m: ModelStatus) => {
    if (!confirm(`Delete ${m.id}? This removes the GGUF file from disk.`)) return;
    setBusy(m.id);
    try {
      await api.remove(m.id);
      refresh();
    } catch (err) {
      alert(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const models = status?.models ?? [];

  return (
    <div className="view">
      <div className="view-header">
        <h1>Models</h1>
        <form className="pull-form" onSubmit={submitPull}>
          <input
            className="pull-input mono"
            placeholder="org/repo:quant"
            value={pullRef}
            onChange={(e) => setPullRef(e.target.value)}
          />
          <button type="submit" className="btn btn-accent">
            Pull
          </button>
        </form>
      </div>

      {pulling?.error && <div className="banner banner-bad">{pulling.error}</div>}
      {pulling && !pulling.error && !models.some((m) => m.id === pulling.id) && (
        <PullingRow
          id={pulling.id}
          onDone={async () => {
            if (localStorage.getItem("nodelm.defaultHotOnPull") === "true") {
              await api.setHot(pulling.id, true).catch(() => {});
            }
            refresh();
          }}
        />
      )}

      <table className="table">
        <thead>
          <tr>
            <th>Model</th>
            <th>Quant</th>
            <th>Size</th>
            <th>RAM verdict</th>
            <th>Resident</th>
            <th></th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {models.map((m) => (
            <tr key={m.id} className="table-row-clickable" onClick={() => setSelected(m.id)}>
              <td>
                <div className="model-name">{m.id}</div>
                {m.aliases.length > 0 && (
                  <div className="model-aliases mono">{m.aliases.join(", ")}</div>
                )}
              </td>
              <td className="mono">{m.quant}</td>
              <td className="mono">{formatBytes(m.size_bytes)}</td>
              <td>
                <VerdictBadge tier={m.verdict_tier} title={m.verdict_reason} />
              </td>
              <td>
                {m.loaded ? (
                  <span className="dot-status">
                    <span className="dot dot-good" />
                    {formatBytes(m.rss_bytes)}
                  </span>
                ) : (
                  <span className="dot-status dim">
                    <span className="dot" />
                    unloaded
                  </span>
                )}
              </td>
              <td>
                <HotToggle on={m.hot} disabled={busy === m.id} onChange={() => toggleHot(m)} />
              </td>
              <td>
                <button
                  type="button"
                  className="btn btn-ghost btn-danger"
                  disabled={busy === m.id}
                  onClick={(e) => {
                    e.stopPropagation();
                    remove(m);
                  }}
                >
                  Delete
                </button>
              </td>
            </tr>
          ))}
          {models.length === 0 && (
            <tr>
              <td colSpan={7} className="table-empty">
                No models downloaded yet — pull one above (e.g. unsloth/Qwen3-30B-A3B:Q6_K).
              </td>
            </tr>
          )}
        </tbody>
      </table>

      {selected && <ModelDetailDrawer id={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

function PullingRow({ id, onDone }: { id: string; onDone: () => void }) {
  const progress = usePullProgress(id);
  useEffect(() => {
    if (progress?.status === "done") onDone();
  }, [progress?.status, onDone]);
  if (!progress || progress.status === "done") return null;
  const percent =
    progress.total_bytes > 0 ? (progress.bytes_downloaded / progress.total_bytes) * 100 : 0;
  return (
    <div className="pull-row">
      <span className="mono">{id}</span>
      {progress.status === "error" ? (
        <span className="banner-bad">{progress.message ?? "download failed"}</span>
      ) : (
        <ProgressBar
          percent={percent}
          label={`${progress.status} — ${formatBytes(progress.bytes_downloaded)} / ${formatBytes(progress.total_bytes)}`}
        />
      )}
    </div>
  );
}
