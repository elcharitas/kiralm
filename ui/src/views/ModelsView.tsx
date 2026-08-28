import { Fragment, useEffect, useState } from "react";
import { Trash2 } from "lucide-react";
import {
  api,
  type CloudLoginStart,
  type CloudPullSuggestion,
  type CloudStatus,
  type ModelStatus,
  type StatusResponse,
} from "../api";
import { HotToggle } from "../components/HotToggle";
import { ProgressBar } from "../components/ProgressBar";
import { useDialogs } from "../components/Dialogs";
import { formatBytes, groupBySection, modelName } from "../format";
import { useSettings } from "../settings";
import { usePullProgress } from "../hooks/usePullProgress";
import { ModelDetailDrawer } from "./ModelDetailDrawer";

export function ModelsView({
  status,
  refresh,
}: {
  status: StatusResponse | null;
  refresh: () => void;
}) {
  const { settings } = useSettings();
  const { confirm, toast } = useDialogs();
  const [pullRef, setPullRef] = useState("");
  const [pulling, setPulling] = useState<{ id: string; error: string | null } | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [catalog, setCatalog] = useState<CloudPullSuggestion[]>([]);

  useEffect(() => {
    api.modelsCatalog().then((d) => setCatalog(d.data), () => {});
  }, []);

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
      const shouldForce = !force && !m.hot && (await confirm({ title: "Load anyway?", message, confirmLabel: "Force load" }));
      if (shouldForce) {
        await toggleHot(m, true);
        return;
      }
      toast(message, "error");
    } finally {
      setBusy(null);
    }
  };

  const remove = async (m: ModelStatus) => {
    const ok = await confirm({
      title: "Delete model",
      message: `Delete ${modelName(m.id)}? This removes the GGUF file from disk.`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    setBusy(m.id);
    try {
      await api.remove(m.id);
      refresh();
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err), "error");
    } finally {
      setBusy(null);
    }
  };

  const models = status?.models ?? [];

  return (
    <div className="view">
      <div className="view-header">
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
            if (settings.default_hot_on_pull) {
              await api.setHot(pulling.id, true).catch(() => {});
            }
            refresh();
          }}
        />
      )}

      <ul className="model-list">
        {groupBySection(models, (m) => m.id).map((g) => (
          <Fragment key={g.section}>
            <li className="model-section-header">{g.section}</li>
            {g.items.map((m) => (
              <li key={m.id} className="model-row model-row-clickable" onClick={() => setSelected(m.id)}>
                <span
                  className={m.source === "cloud" ? "model-row-dot" : m.loaded ? "model-row-dot model-row-dot-on" : "model-row-dot"}
                  title={m.source === "cloud" ? "Cloud" : m.loaded ? `Resident: ${formatBytes(m.rss_bytes)}` : "Not loaded"}
                />
                <span className="model-row-main">
                  <span className="model-row-name">{modelName(m.id)}</span>
                  <span className="model-row-size mono">{formatBytes(m.size_bytes)}</span>
                </span>
                <span className="model-row-actions">
                  {m.source === "local" && (
                    <>
                      <HotToggle on={m.hot} disabled={busy === m.id} onChange={() => toggleHot(m)} />
                      <button
                        type="button"
                        className="model-row-delete-btn"
                        title="Delete"
                        disabled={busy === m.id}
                        onClick={(e) => {
                          e.stopPropagation();
                          remove(m);
                        }}
                      >
                        <Trash2 size={14} />
                      </button>
                    </>
                  )}
                </span>
              </li>
            ))}
          </Fragment>
        ))}
        {models.length === 0 && <li className="model-list-empty">No models downloaded yet. Pull one below.</li>}
        {catalog.length === 0 && (
          <li className="model-list-empty">No pull suggestions synced yet.</li>
        )}
        {groupBySection(
          catalog.filter((c) => !models.some((m) => m.id === c.ref || m.aliases.includes(c.ref))),
          (c) => c.ref,
        ).map((g) => (
          <Fragment key={g.section}>
            <li className="model-section-header">{g.section}</li>
            {g.items.map((c) => (
              <li key={c.ref} className="model-row" title={c.description ?? undefined}>
                <span className="model-row-dot" />
                <span className="model-row-main">
                  <span className="model-row-name">{c.name}</span>
                  <span className="model-row-size mono">{formatBytes(c.size_bytes)}</span>
                </span>
                <span className="model-row-actions">
                  <button type="button" className="btn btn-ghost" onClick={() => pull(c.ref)}>
                    Pull
                  </button>
                </span>
              </li>
            ))}
          </Fragment>
        ))}
      </ul>

      {selected && <ModelDetailDrawer id={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

// Login affordance for cloud models (kept to this view's single dense
// table, no separate cloud section elsewhere).
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
      {progress?.phase === "expired" && <span className="dim">Code expired. Try again.</span>}
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
          label={`${progress.status}: ${formatBytes(progress.bytes_downloaded)} / ${formatBytes(progress.total_bytes)}`}
        />
      )}
    </div>
  );
}
