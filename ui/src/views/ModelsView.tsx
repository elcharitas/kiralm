import { useEffect, useState } from "react";
import {
  api,
  type CloudLoginStart,
  type CloudStatus,
  type ModelStatus,
  type StatusResponse,
} from "../api";
import { VerdictBadge } from "../components/VerdictBadge";
import { HotToggle } from "../components/HotToggle";
import { ProgressBar } from "../components/ProgressBar";
import { formatBytes } from "../format";
import { usePullProgress } from "../hooks/usePullProgress";
import { ModelDetailDrawer } from "./ModelDetailDrawer";

// ponytail: plain hardcoded list, not a live catalog — edit this array to add/update entries.
const CATALOG: { name: string; ref: string; size: string; description: string }[] = [
  {
    name: "Qwen3 0.6B",
    ref: "unsloth/Qwen3-0.6B-GGUF:Q8_0",
    size: "~0.7 GB",
    description: "Tiny dense model, runs anywhere, good for quick tests.",
  },
  {
    name: "Llama 3.2 1B Instruct",
    ref: "unsloth/Llama-3.2-1B-Instruct-GGUF:Q8_0",
    size: "~1.3 GB",
    description: "Small dense general model for constrained machines.",
  },
  {
    name: "Qwen3 4B Instruct",
    ref: "unsloth/Qwen3-4B-Instruct-2507-GGUF:Q4_K_M",
    size: "~2.5 GB",
    description: "Solid everyday dense model, cheap to keep hot.",
  },
  {
    name: "Llama 3.1 8B Instruct",
    ref: "unsloth/Meta-Llama-3.1-8B-Instruct-GGUF:Q4_K_M",
    size: "~4.9 GB",
    description: "Popular general-purpose dense model.",
  },
  {
    name: "Qwen2.5 14B Instruct",
    ref: "unsloth/Qwen2.5-14B-Instruct-GGUF:Q4_K_M",
    size: "~9 GB",
    description: "Larger dense model for stronger reasoning.",
  },
  {
    name: "OLMoE 1B-7B",
    ref: "unsloth/OLMoE-1B-7B-0924-Instruct-GGUF:Q4_K_M",
    size: "~4.3 GB",
    description: "MoE model: 7B total, ~1B active — the over-RAM story.",
  },
  {
    name: "Qwen3 30B-A3B",
    ref: "unsloth/Qwen3-30B-A3B-GGUF:Q4_K_M",
    size: "~18 GB",
    description: "MoE model: 30B total, ~3B active — runs on modest RAM.",
  },
  {
    name: "Mixtral 8x7B Instruct",
    ref: "TheBloke/Mixtral-8x7B-Instruct-v0.1-GGUF:Q4_K_M",
    size: "~26 GB",
    description: "Larger MoE model, needs more RAM but still under-dense-equivalent.",
  },
];

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

  const pull = async (ref: string) => {
    try {
      const { id } = await api.pull(ref);
      setPulling({ id, error: null });
      setPullRef("");
    } catch (err) {
      setPulling({ id: ref, error: err instanceof Error ? err.message : String(err) });
    }
  };

  const submitPull = async (e: React.FormEvent) => {
    e.preventDefault();
    const ref = pullRef.trim();
    if (!ref) return;
    await pull(ref);
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

      <CloudLoginPanel onChange={refresh} />

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
                {m.source === "cloud" ? (
                  <span className="badge badge-cloud">Cloud</span>
                ) : (
                  <VerdictBadge tier={m.verdict_tier!} title={m.verdict_reason ?? undefined} />
                )}
              </td>
              <td>
                {m.source === "cloud" ? (
                  <span className="dim">—</span>
                ) : m.loaded ? (
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
                {m.source === "local" && (
                  <HotToggle on={m.hot} disabled={busy === m.id} onChange={() => toggleHot(m)} />
                )}
              </td>
              <td>
                {m.source === "local" && (
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
                )}
              </td>
            </tr>
          ))}
          {models.length === 0 && (
            <tr>
              <td colSpan={7} className="table-empty">
                No models downloaded yet — pull one below.
              </td>
            </tr>
          )}
          {CATALOG.filter(
            (c) => !models.some((m) => m.id === c.ref || m.aliases.includes(c.ref))
          ).map((c) => (
            <tr key={c.ref} className="table-row-catalog">
              <td>
                <div className="model-name">{c.name}</div>
                <div className="model-aliases mono">{c.ref}</div>
              </td>
              <td className="mono">{c.ref.split(":").pop()}</td>
              <td className="mono">{c.size}</td>
              <td className="dim">{c.description}</td>
              <td className="dim">not downloaded</td>
              <td colSpan={2}>
                <button type="button" className="btn btn-ghost" onClick={() => pull(c.ref)}>
                  Pull
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {selected && <ModelDetailDrawer id={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

// Login affordance for cloud models (kept to this view, per SPEC's
// single-dense-table rule — no separate cloud section elsewhere).
function CloudLoginPanel({ onChange }: { onChange: () => void }) {
  const [status, setStatus] = useState<CloudStatus | null>(null);
  const [start, setStart] = useState<CloudLoginStart | null>(null);
  const [progress, setProgress] = useState<{ phase: string; message: string | null } | null>(
    null,
  );
  const [busy, setBusy] = useState(false);

  const loadStatus = () => api.cloudStatus().then(setStatus, () => {});

  useEffect(() => {
    loadStatus();
  }, []);

  useEffect(() => {
    if (!start) return;
    const source = new EventSource(api.cloudLoginProgressUrl(start.device_code));
    source.onmessage = (ev) => {
      try {
        const data = JSON.parse(ev.data) as { phase: string; message: string | null };
        setProgress(data);
        if (data.phase === "success") {
          source.close();
          setStart(null);
          loadStatus();
          onChange();
        } else if (data.phase === "expired" || data.phase === "error") {
          source.close();
        }
      } catch {
        // ignore malformed frame, next one carries fresh state
      }
    };
    source.onerror = () => source.close();
    return () => source.close();
  }, [start]);

  const login = async () => {
    setBusy(true);
    setProgress(null);
    try {
      setStart(await api.cloudLogin());
    } catch (err) {
      setProgress({ phase: "error", message: err instanceof Error ? err.message : String(err) });
    } finally {
      setBusy(false);
    }
  };

  const logout = async () => {
    setBusy(true);
    try {
      await api.cloudLogout();
      await loadStatus();
      onChange();
    } finally {
      setBusy(false);
    }
  };

  if (!status) return null;

  if (status.logged_in) {
    return (
      <div className="banner banner-info cloud-login-banner">
        <span>Cloud: logged in{status.device ? ` as ${status.device}` : ""}</span>
        <button type="button" className="btn btn-ghost" disabled={busy} onClick={logout}>
          Log out
        </button>
      </div>
    );
  }

  if (start && progress?.phase !== "expired" && progress?.phase !== "error") {
    const link = start.verification_uri_complete ?? start.verification_uri;
    return (
      <div className="banner banner-info cloud-login-banner">
        <span>
          Enter code <strong className="mono">{start.user_code}</strong> at{" "}
          <a href={link} target="_blank" rel="noreferrer">
            {link}
          </a>
        </span>
        <span className="dim">Waiting for approval…</span>
      </div>
    );
  }

  return (
    <div className="banner banner-info cloud-login-banner">
      <span>Log in for cloud models</span>
      {progress?.phase === "expired" && <span className="dim">Code expired — try again.</span>}
      {progress?.phase === "error" && <span className="banner-bad">{progress.message}</span>}
      <button type="button" className="btn btn-accent" disabled={busy} onClick={login}>
        Log in
      </button>
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
