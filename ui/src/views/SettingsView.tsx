import { useCallback, useEffect, useState } from "react";
import {
  api,
  type AppSettings,
  type DiagnosticsResponse,
  type PermissionMode,
  type Project,
  type StatusResponse,
  type TunnelStatus,
  type WorkspacePreset,
  type WorkspacePresetInput,
} from "../api";
import { ModelsView } from "./ModelsView";
import { EvaluationsView } from "./EvaluationsView";
import { useDialogs } from "../components/Dialogs";
import { Select } from "../components/Select";
import { useSettings } from "../settings";
import { formatBytes } from "../format";

export type SettingsTab = "general" | "models" | "evaluations";

// Local Font Access API -- not in lib.dom.d.ts yet. KiraLM only runs
// as a Tauri desktop webview, never a public site, so there's no
// stranger-loads-a-webpage privacy case to gate this behind a click.
interface LocalFontData {
  family: string;
}
declare global {
  interface Window {
    queryLocalFonts?: () => Promise<LocalFontData[]>;
  }
}

// Deduped since queryLocalFonts returns one entry per style (e.g.
// "Arial", "Arial" again for bold).
function useSystemFonts() {
  const [fonts, setFonts] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const supported = typeof window.queryLocalFonts === "function";

  useEffect(() => {
    if (!supported) return;
    window
      .queryLocalFonts!()
      .then((list) => setFonts(Array.from(new Set(list.map((f) => f.family))).sort()))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, [supported]);

  return { fonts, error, supported };
}

export function SettingsView({
  status,
  refresh,
  tab,
  onTabChange,
}: {
  status: StatusResponse | null;
  refresh: () => void;
  tab: SettingsTab;
  onTabChange: (tab: SettingsTab) => void;
}) {
  if (tab === "models") {
    return (
      <div className="settings-wrap">
        <div className="settings-tabs">
          <button type="button" className="settings-tab" onClick={() => onTabChange("general")}>
            General
          </button>
          <button type="button" className="settings-tab settings-tab-active">
            Models
          </button>
          <button type="button" className="settings-tab" onClick={() => onTabChange("evaluations")}>
            Evaluations
          </button>
        </div>
        <ModelsView status={status} refresh={refresh} />
      </div>
    );
  }

  if (tab === "evaluations") {
    return (
      <div className="settings-wrap">
        <div className="settings-tabs">
          <button type="button" className="settings-tab" onClick={() => onTabChange("general")}>
            General
          </button>
          <button type="button" className="settings-tab" onClick={() => onTabChange("models")}>
            Models
          </button>
          <button type="button" className="settings-tab settings-tab-active">
            Evaluations
          </button>
        </div>
        <EvaluationsView status={status} />
      </div>
    );
  }

  return (
    <div className="settings-wrap">
      <div className="settings-tabs">
        <button type="button" className="settings-tab settings-tab-active">
          General
        </button>
        <button type="button" className="settings-tab" onClick={() => onTabChange("models")}>
          Models
        </button>
        <button type="button" className="settings-tab" onClick={() => onTabChange("evaluations")}>
          Evaluations
        </button>
      </div>

      <div className="settings-scroll">
        <GeneralSettingsForm status={status} />
        <WorkspacePresetsSection status={status} />
        <DiagnosticsSection />
        <RemoteAccessSection />
        <DataSection />
      </div>
    </div>
  );
}

function DiagnosticsSection() {
  const [diagnostics, setDiagnostics] = useState<DiagnosticsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setDiagnostics(await api.diagnostics());
      setError(null);
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const report = diagnostics?.platform;
  const health = diagnostics?.telemetry.health;

  return (
    <section className="settings-section">
      <div className="diagnostics-heading">
        <div>
          <h3>Diagnostics and observability</h3>
          <p className="settings-hint">
            Local readiness checks, bounded metrics, and redacted recent errors.
          </p>
        </div>
        <button type="button" className="btn" disabled={loading} onClick={() => void load()}>
          {loading ? "Checking..." : "Run checks"}
        </button>
      </div>
      {error && <p className="diagnostics-error">{error}</p>}
      {report && health && (
        <>
          <div className="diagnostics-summary">
            <span>
              <strong>{health.status}</strong>
              {health.ready ? " · ready" : " · not ready"}
            </span>
            <span>
              {report.hardware.platform} · {report.hardware.architecture}
            </span>
            <span>{report.hardware.logical_cpus} logical CPUs</span>
            <span>{formatBytes(report.hardware.available_memory_bytes)} RAM available</span>
          </div>
          <div className="diagnostics-grid">
            <div>
              <h4>Managed paths</h4>
              {report.paths.map((path) => (
                <div className="diagnostics-row" key={path.kind}>
                  <span>{path.kind}</span>
                  <span className="mono truncate" title={path.path}>{path.path}</span>
                  <span className={path.writable ? "diagnostics-ok" : "diagnostics-bad"}>
                    {path.writable ? formatBytes(path.available_bytes) : "not writable"}
                  </span>
                </div>
              ))}
            </div>
            <div>
              <h4>Executables</h4>
              {report.executables.map((executable) => (
                <div className="diagnostics-row" key={executable.name}>
                  <span className="mono truncate" title={executable.name}>{executable.name}</span>
                  <span className={executable.found ? "diagnostics-ok" : "diagnostics-bad"}>
                    {executable.found ? "found" : executable.required ? "required" : "optional"}
                  </span>
                </div>
              ))}
            </div>
          </div>
          {report.findings.length > 0 && (
            <div className="diagnostics-findings">
              {report.findings.map((finding, index) => (
                <div className={`diagnostics-finding diagnostics-${finding.severity}`} key={`${finding.code}:${index}`}>
                  <strong>{finding.summary}</strong>
                  <span>{finding.action}</span>
                </div>
              ))}
            </div>
          )}
          <p className="settings-hint diagnostics-stats">
            {diagnostics.telemetry.stats.series} metric series · {diagnostics.telemetry.stats.total_errors} recent errors · {diagnostics.telemetry.stats.dropped_series} dropped series
          </p>
        </>
      )}
    </section>
  );
}

// Every editable setting on this page -- daemon config (config.toml)
// and preferences (settings.json) -- shares one draft and one Save,
// so there's exactly one save action on the whole page. Remote access
// and Data below are real-time actions (enable/disable, export/clear).
function GeneralSettingsForm({ status }: { status: StatusResponse | null }) {
  const { settings, save } = useSettings();
  const { toast } = useDialogs();

  const [ramHeadroom, setRamHeadroom] = useState("");
  const [modelDir, setModelDir] = useState("");
  const [cacheQuota, setCacheQuota] = useState("50");
  const [optimizationProfile, setOptimizationProfile] = useState<
    StatusResponse["config"]["optimization_profile"]
  >("balanced");
  const [configTouched, setConfigTouched] = useState(false);
  const [notifPermission, setNotifPermission] = useState<NotificationPermission | "unsupported">(
    typeof Notification === "undefined" ? "unsupported" : Notification.permission,
  );

  // Prefill from the real daemon config once it's loaded, but stop
  // overwriting whatever the user's actively typing.
  useEffect(() => {
    if (configTouched || !status) return;
    setRamHeadroom(status.config.ram_headroom_gb != null ? String(status.config.ram_headroom_gb) : "");
    setModelDir(status.config.model_dir);
    setCacheQuota(String(status.config.cache_quota_gb));
    setOptimizationProfile(status.config.optimization_profile);
  }, [status, configTouched]);

  // Writes config.toml on blur, not every keystroke -- there's no Save
  // button, but a text field still needs somewhere to land mid-edit.
  const saveConfig = async (
    profile = optimizationProfile,
    force = false,
  ) => {
    if (!configTouched && !force) return;
    const trimmed = ramHeadroom.trim();
    const gb = trimmed === "" ? null : Number(trimmed);
    if (gb !== null && !Number.isFinite(gb)) {
      toast("RAM headroom must be a number", "error");
      return;
    }
    const quota = Number(cacheQuota);
    if (!Number.isFinite(quota) || quota < 1) {
      toast("Cache quota must be at least 1GB", "error");
      return;
    }
    try {
      await api.updateConfig(gb, modelDir, quota, profile);
      setConfigTouched(false);
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  // Every other field here is a checkbox/select -- a real, discrete
  // choice, not free text -- so it saves the instant it changes.
  const patchSettings = async (patch: Partial<typeof settings>) => {
    try {
      await save({ ...settings, ...patch });
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const { fonts: systemFonts, error: fontsError, supported: fontsSupported } = useSystemFonts();

  const toggleNotify = async (checked: boolean) => {
    if (checked && notifPermission === "default") {
      const result = await Notification.requestPermission();
      setNotifPermission(result);
      if (result !== "granted") return;
    }
    patchSettings({ notify_on_finish: checked });
  };

  return (
    <>
      <section className="settings-section">
        <h3>Daemon config</h3>
        <p className="settings-hint">
          Written to <code>~/.kiralm/config.toml</code> as you edit. The daemon loads this once at
          startup, so restart it to apply changes.
        </p>
        <div className="settings-field">
          <label htmlFor="ram-headroom">RAM headroom (GB)</label>
          <input
            id="ram-headroom"
            className="settings-input"
            placeholder="auto"
            value={ramHeadroom}
            onChange={(e) => {
              setRamHeadroom(e.target.value);
              setConfigTouched(true);
            }}
            onBlur={() => void saveConfig()}
          />
        </div>
        <div className="settings-field">
          <label htmlFor="cache-quota">Artifact cache quota (GB)</label>
          <input
            id="cache-quota"
            className="settings-input"
            value={cacheQuota}
            onChange={(e) => {
              setCacheQuota(e.target.value);
              setConfigTouched(true);
            }}
            onBlur={() => void saveConfig()}
          />
        </div>
        <div className="settings-field">
          <label>Runtime optimization</label>
          <Select
            triggerClassName="settings-select"
            value={optimizationProfile}
            onChange={(value) => {
              const profile = value as StatusResponse["config"]["optimization_profile"];
              setOptimizationProfile(profile);
              void saveConfig(profile, true);
            }}
            options={[
              { value: "balanced", label: "Balanced" },
              { value: "throughput", label: "Throughput" },
              { value: "latency", label: "Low latency" },
              { value: "memory", label: "Memory saver" },
            ]}
          />
        </div>
        <div className="settings-field">
          <label htmlFor="model-dir">Model directory</label>
          <input
            id="model-dir"
            className="settings-input mono"
            value={modelDir}
            onChange={(e) => {
              setModelDir(e.target.value);
              setConfigTouched(true);
            }}
            onBlur={() => void saveConfig()}
          />
        </div>
      </section>

      <section className="settings-section">
        <h3>New chat defaults</h3>
        <p className="settings-hint">
          Saved to <code>~/.kiralm/settings.json</code> as you change them.
        </p>
        <div className="settings-field">
          <label>Default tool access</label>
          <Select
            triggerClassName="settings-select"
            value={settings.default_permission}
            onChange={(value) => patchSettings({ default_permission: value as AppSettings["default_permission"] })}
            options={[
              { value: "read-only", label: "Read only" },
              { value: "ask", label: "Ask before changes" },
              { value: "full-access", label: "Full access" },
            ]}
          />
        </div>
        <div className="settings-field">
          <label htmlFor="global-agent-instructions">Global agent instructions</label>
          <textarea
            id="global-agent-instructions"
            className="settings-input settings-instructions"
            defaultValue={settings.custom_instructions}
            maxLength={16 * 1024}
            placeholder="Preferences that should apply to every workspace and chat"
            onBlur={(event) => {
              if (event.currentTarget.value !== settings.custom_instructions) {
                void patchSettings({ custom_instructions: event.currentTarget.value });
              }
            }}
          />
          <p className="settings-hint">
            Applied after KiraLM’s core agent rules and before workspace or chat instructions.
          </p>
        </div>
        <label className="settings-toggle">
          <span>Enter sends the message (unchecked: Enter for a newline, Ctrl/Cmd+Enter sends)</span>
          <input
            type="checkbox"
            className="switch-input"
            checked={settings.send_on_enter}
            onChange={(e) => patchSettings({ send_on_enter: e.target.checked })}
          />
        </label>
        <label className="settings-toggle">
          <span>Mark newly pulled models Hot by default</span>
          <input
            type="checkbox"
            className="switch-input"
            checked={settings.default_hot_on_pull}
            onChange={(e) => patchSettings({ default_hot_on_pull: e.target.checked })}
          />
        </label>
        <label className="settings-toggle">
          <span>Automatically compact long chats (summarize older messages instead of resending them)</span>
          <input
            type="checkbox"
            className="switch-input"
            checked={settings.auto_compact}
            onChange={(e) => patchSettings({ auto_compact: e.target.checked })}
          />
        </label>
      </section>

      <section className="settings-section">
        <h3>Notifications</h3>
        <label className="settings-toggle">
          <span>Notify when a background chat finishes</span>
          <input
            type="checkbox"
            className="switch-input"
            checked={settings.notify_on_finish}
            disabled={notifPermission === "unsupported" || notifPermission === "denied"}
            onChange={(e) => toggleNotify(e.target.checked)}
          />
        </label>
        {notifPermission === "denied" && (
          <p className="settings-hint">Notifications are blocked for this app in your browser/OS settings.</p>
        )}
      </section>

      <section className="settings-section">
        <h3>Appearance</h3>
        <div className="settings-field">
          <label>Line height</label>
          <Select
            triggerClassName="settings-select"
            value={settings.line_height}
            onChange={(v) => patchSettings({ line_height: v as AppSettings["line_height"] })}
            options={[
              { value: "compact", label: "Compact" },
              { value: "normal", label: "Normal" },
              { value: "relaxed", label: "Relaxed" },
            ]}
          />
        </div>
        <div className="settings-field">
          <label>Font</label>
          <Select
            triggerClassName="settings-select"
            value={settings.font}
            onChange={(v) => patchSettings({ font: v })}
            options={[
              { value: "default", label: "KiraLM Default" },
              ...(systemFonts ?? []).map((f) => ({ value: f, label: f })),
            ]}
          />
          {!fontsSupported && (
            <p className="settings-hint">
              This app can't list system fonts here, so only KiraLM Default is available.
            </p>
          )}
          {fontsError && <p className="settings-hint">{fontsError}</p>}
        </div>
      </section>
    </>
  );
}

function emptyPreset(): WorkspacePresetInput {
  return {
    name: "",
    project_id: null,
    preferred_model: null,
    permission: "read-only",
    optimization_profile: "balanced",
    knowledge_roots: [],
    hot_model_ids: [],
    enabled_mcp_servers: [],
    instruction_text: "",
  };
}

function splitValues(value: string): string[] {
  return value
    .split(",")
    .map((item) => item.trim())
    .filter(Boolean);
}

function WorkspacePresetsSection({ status }: { status: StatusResponse | null }) {
  const { confirm, toast } = useDialogs();
  const [presets, setPresets] = useState<WorkspacePreset[]>([]);
  const [storeRevision, setStoreRevision] = useState(0);
  const [projects, setProjects] = useState<Project[]>([]);
  const [editing, setEditing] = useState<WorkspacePreset | null>(null);
  const [draft, setDraft] = useState<WorkspacePresetInput>(emptyPreset);
  const [busy, setBusy] = useState(false);

  const reload = async () => {
    const [presetResult, projectResult] = await Promise.all([
      api.listWorkspacePresets(),
      api.listProjects(),
    ]);
    setPresets(presetResult.presets);
    setStoreRevision(presetResult.revision);
    setProjects(projectResult.projects);
  };

  useEffect(() => {
    reload().catch(() => {});
  }, []);

  const patchDraft = (patch: Partial<WorkspacePresetInput>) => {
    setDraft((current) => ({ ...current, ...patch }));
  };

  const reset = () => {
    setEditing(null);
    setDraft(emptyPreset());
  };

  const savePreset = async () => {
    setBusy(true);
    try {
      if (editing) {
        await api.updateWorkspacePreset({ ...editing, ...draft });
        toast("Workspace preset updated", "success");
      } else {
        await api.createWorkspacePreset(draft);
        toast("Workspace preset created", "success");
      }
      reset();
      await reload();
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    } finally {
      setBusy(false);
    }
  };

  const removePreset = async (preset: WorkspacePreset) => {
    const approved = await confirm({
      title: "Delete workspace preset",
      message: `Delete “${preset.name}”?`,
      confirmLabel: "Delete preset",
      danger: true,
    });
    if (!approved) return;
    try {
      await api.deleteWorkspacePreset(preset.id, preset.revision);
      if (editing?.id === preset.id) reset();
      await reload();
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  const duplicatePreset = async (preset: WorkspacePreset) => {
    try {
      await api.duplicateWorkspacePreset(
        preset.id,
        preset.revision,
        `${preset.name} copy`,
      );
      await reload();
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  const exportPresets = async () => {
    try {
      const body = await api.exportWorkspacePresets();
      const url = URL.createObjectURL(
        new Blob([body], { type: "application/json" }),
      );
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = "kiralm-workspace-presets.json";
      anchor.click();
      URL.revokeObjectURL(url);
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  const importPresets = async (file: File | undefined) => {
    if (!file) return;
    try {
      await api.importWorkspacePresets(
        await file.text(),
        "merge",
        storeRevision,
      );
      await reload();
      toast("Workspace presets imported", "success");
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  return (
    <section className="settings-section">
      <h3>Workspace presets</h3>
      <p className="settings-hint">
        Reuse model, access, optimization, knowledge, MCP, and instruction
        settings across workspaces.
      </p>
      {presets.length > 0 && (
        <div className="preset-list">
          {presets.map((preset) => (
            <div className="preset-row" key={preset.id}>
              <div>
                <strong>{preset.name}</strong>
                <span>
                  {projects.find((project) => project.id === preset.project_id)
                    ?.name ?? "Any workspace"}
                </span>
              </div>
              <div className="settings-actions">
                <button
                  type="button"
                  className="btn"
                  onClick={() => {
                    setEditing(preset);
                    setDraft({
                      name: preset.name,
                      project_id: preset.project_id,
                      preferred_model: preset.preferred_model,
                      permission: preset.permission,
                      optimization_profile: preset.optimization_profile,
                      knowledge_roots: preset.knowledge_roots,
                      hot_model_ids: preset.hot_model_ids,
                      enabled_mcp_servers: preset.enabled_mcp_servers,
                      instruction_text: preset.instruction_text,
                    });
                  }}
                >
                  Edit
                </button>
                <button
                  type="button"
                  className="btn"
                  onClick={() => void duplicatePreset(preset)}
                >
                  Duplicate
                </button>
                <button
                  type="button"
                  className="btn btn-danger-solid"
                  onClick={() => void removePreset(preset)}
                >
                  Delete
                </button>
              </div>
            </div>
          ))}
        </div>
      )}
      <div className="settings-field">
        <label htmlFor="preset-name">Name</label>
        <input
          id="preset-name"
          className="settings-input"
          value={draft.name}
          onChange={(event) => patchDraft({ name: event.target.value })}
        />
      </div>
      <div className="settings-field">
        <label>Workspace</label>
        <Select
          triggerClassName="settings-select"
          value={draft.project_id ?? ""}
          onChange={(value) => patchDraft({ project_id: value || null })}
          options={[
            { value: "", label: "Any workspace" },
            ...projects.map((project) => ({
              value: project.id,
              label: project.name,
            })),
          ]}
        />
      </div>
      <div className="settings-field">
        <label>Preferred model</label>
        <Select
          triggerClassName="settings-select"
          value={draft.preferred_model ?? ""}
          onChange={(value) => patchDraft({ preferred_model: value || null })}
          options={[
            { value: "", label: "Automatic" },
            ...(status?.models ?? []).map((model) => ({
              value: model.id,
              label: model.id,
            })),
          ]}
        />
      </div>
      <div className="settings-field">
        <label>Tool access</label>
        <Select
          triggerClassName="settings-select"
          value={draft.permission}
          onChange={(value) =>
            patchDraft({ permission: value as PermissionMode })
          }
          options={[
            { value: "read-only", label: "Read only" },
            { value: "ask", label: "Ask before changes" },
            { value: "full-access", label: "Full access" },
          ]}
        />
      </div>
      <div className="settings-field">
        <label>Runtime optimization</label>
        <Select
          triggerClassName="settings-select"
          value={draft.optimization_profile}
          onChange={(value) =>
            patchDraft({
              optimization_profile:
                value as WorkspacePresetInput["optimization_profile"],
            })
          }
          options={[
            { value: "balanced", label: "Balanced" },
            { value: "throughput", label: "Throughput" },
            { value: "latency", label: "Low latency" },
            { value: "memory", label: "Memory saver" },
          ]}
        />
      </div>
      <div className="settings-field">
        <label htmlFor="preset-roots">Extra knowledge roots</label>
        <input
          id="preset-roots"
          className="settings-input mono"
          placeholder="Comma-separated absolute paths"
          value={draft.knowledge_roots.join(", ")}
          onChange={(event) =>
            patchDraft({ knowledge_roots: splitValues(event.target.value) })
          }
        />
      </div>
      <div className="settings-field">
        <label htmlFor="preset-hot-models">Hot model IDs</label>
        <input
          id="preset-hot-models"
          className="settings-input mono"
          placeholder="Comma-separated model IDs"
          value={draft.hot_model_ids.join(", ")}
          onChange={(event) =>
            patchDraft({ hot_model_ids: splitValues(event.target.value) })
          }
        />
      </div>
      <div className="settings-field">
        <label htmlFor="preset-mcp">Enabled MCP servers</label>
        <input
          id="preset-mcp"
          className="settings-input mono"
          placeholder="Comma-separated server names"
          value={draft.enabled_mcp_servers.join(", ")}
          onChange={(event) =>
            patchDraft({
              enabled_mcp_servers: splitValues(event.target.value),
            })
          }
        />
      </div>
      <div className="settings-field">
        <label htmlFor="preset-instructions">Workspace instructions</label>
        <textarea
          id="preset-instructions"
          className="settings-input"
          rows={4}
          value={draft.instruction_text}
          onChange={(event) =>
            patchDraft({ instruction_text: event.target.value })
          }
        />
      </div>
      <div className="settings-actions">
        <button
          type="button"
          className="btn btn-accent"
          disabled={busy || !draft.name.trim()}
          onClick={() => void savePreset()}
        >
          {editing ? "Update preset" : "Create preset"}
        </button>
        {editing && (
          <button type="button" className="btn" onClick={reset}>
            Cancel edit
          </button>
        )}
        <button type="button" className="btn" onClick={exportPresets}>
          Export
        </button>
        <label className="btn preset-import">
          Import
          <input
            type="file"
            accept="application/json,.json"
            onChange={(event) =>
              void importPresets(event.target.files?.[0])
            }
          />
        </label>
      </div>
    </section>
  );
}

// Real `kiralm tunnel enable/disable/status` over HTTP -- Cloudflare
// Tunnel remote access, previously CLI-only.
function RemoteAccessSection() {
  const { toast } = useDialogs();
  const [status, setStatus] = useState<TunnelStatus | null>(null);
  const [busy, setBusy] = useState(false);

  const loadStatus = () => api.tunnelStatus().then(setStatus, () => {});

  useEffect(() => {
    loadStatus();
  }, []);

  const enable = async () => {
    setBusy(true);
    try {
      await api.tunnelEnable();
      await loadStatus();
      toast("Tunnel enabled", "success");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const disable = async () => {
    setBusy(true);
    try {
      await api.tunnelDisable();
      await loadStatus();
      toast("Tunnel disabled", "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="settings-section">
      <h3>Remote access</h3>
      <p className="settings-hint">
        A Cloudflare Tunnel to this daemon, bearer-checked. Lets you reach it from outside this
        machine without opening a port.
      </p>
      {status?.enabled ? (
        <>
          <dl className="detail-grid settings-detail-grid">
            <dt>Hostname</dt>
            <dd className="mono">{status.hostname ?? "-"}</dd>
            <dt>Status</dt>
            <dd>
              <span className={status.running ? "badge badge-good" : "badge badge-bad"}>
                {status.running ? "Running" : "Not running"}
              </span>
              {status.connectivity_confirmed && <span className="badge badge-good">Reachable</span>}
            </dd>
          </dl>
          <button type="button" className="btn btn-danger-solid" disabled={busy} onClick={disable}>
            {busy ? "Disabling…" : "Disable tunnel"}
          </button>
        </>
      ) : (
        <button type="button" className="btn btn-accent" disabled={busy} onClick={enable}>
          {busy ? "Enabling…" : "Enable tunnel"}
        </button>
      )}
    </section>
  );
}

function DataSection() {
  const { confirm, toast } = useDialogs();

  const exportAll = async () => {
    const { chats } = await api.listChats();
    const blob = new Blob([JSON.stringify(chats, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = "kiralm-chats.json";
    a.click();
    URL.revokeObjectURL(url);
  };

  const clearAll = async () => {
    const ok = await confirm({
      title: "Clear all chats",
      message: "Deletes every chat on this device. This can't be undone.",
      confirmLabel: "Delete everything",
      danger: true,
    });
    if (!ok) return;
    const { chats } = await api.listChats();
    await Promise.all(chats.map((c) => api.deleteChat(c.id)));
    toast("All chats cleared", "info");
    window.location.reload();
  };

  return (
    <section className="settings-section">
      <h3>Data</h3>
      <p className="settings-hint">
        Chats live on this daemon (~/.kiralm/chats.json), not just this browser.
      </p>
      <div className="settings-actions">
        <button type="button" className="btn" onClick={exportAll}>
          Export all chats
        </button>
        <button type="button" className="btn btn-danger-solid" onClick={clearAll}>
          Clear all chats
        </button>
      </div>
    </section>
  );
}
