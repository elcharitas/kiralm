import { useEffect, useState } from "react";
import { api, type AppSettings, type StatusResponse, type TunnelStatus } from "../api";
import { ModelsView } from "./ModelsView";
import { useDialogs } from "../components/Dialogs";
import { Select } from "../components/Select";
import { useSettings } from "../settings";

export type SettingsTab = "general" | "models";

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
        </div>
        <ModelsView status={status} refresh={refresh} />
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
      </div>

      <div className="settings-scroll">
        <GeneralSettingsForm status={status} />
        <RemoteAccessSection />
        <DataSection />
      </div>
    </div>
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
  }, [status, configTouched]);

  // Writes config.toml on blur, not every keystroke -- there's no Save
  // button, but a text field still needs somewhere to land mid-edit.
  const saveConfig = async () => {
    if (!configTouched) return;
    const trimmed = ramHeadroom.trim();
    const gb = trimmed === "" ? null : Number(trimmed);
    if (gb !== null && !Number.isFinite(gb)) {
      toast("RAM headroom must be a number", "error");
      return;
    }
    try {
      await api.updateConfig(gb, modelDir);
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
            onBlur={saveConfig}
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
            onBlur={saveConfig}
          />
        </div>
      </section>

      <section className="settings-section">
        <h3>New chat defaults</h3>
        <p className="settings-hint">
          Saved to <code>~/.kiralm/settings.json</code> as you change them.
        </p>
        <label className="settings-toggle">
          <span>Start new chats with full access instead of read-only</span>
          <input
            type="checkbox"
            className="switch-input"
            checked={settings.default_permission === "full-access"}
            onChange={(e) => patchSettings({ default_permission: e.target.checked ? "full-access" : "read-only" })}
          />
        </label>
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
