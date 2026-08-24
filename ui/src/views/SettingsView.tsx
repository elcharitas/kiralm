import { useState } from "react";
import type { StatusResponse } from "../api";
import { formatBytes } from "../format";

const DEFAULT_HOT_KEY = "kiralm.defaultHotOnPull";

export function SettingsView({ status }: { status: StatusResponse | null }) {
  const [defaultHot, setDefaultHot] = useState(
    () => localStorage.getItem(DEFAULT_HOT_KEY) === "true",
  );

  const toggleDefaultHot = () => {
    const next = !defaultHot;
    setDefaultHot(next);
    localStorage.setItem(DEFAULT_HOT_KEY, String(next));
  };

  return (
    <div className="view">
      <div className="view-header">
        <h1>Settings</h1>
      </div>

      <section className="settings-section">
        <h3>Daemon config</h3>
        <p className="settings-hint">
          Read from <code>~/.kiralm/config.toml</code>, hand-edited. Restart the daemon after
          changing it on disk.
        </p>
        <dl className="detail-grid">
          <dt>RAM headroom</dt>
          <dd className="mono">{status?.config.ram_headroom_gb ?? "-"} GB</dd>

          <dt>Model directory</dt>
          <dd className="mono">{status?.config.model_dir ?? "-"}</dd>

          <dt>Total RAM</dt>
          <dd className="mono">{formatBytes(status?.ram.total_bytes)}</dd>

          <dt>Available RAM</dt>
          <dd className="mono">{formatBytes(status?.ram.available_bytes)}</dd>

          <dt>Reserved headroom</dt>
          <dd className="mono">{formatBytes(status?.ram.headroom_bytes)}</dd>
        </dl>
      </section>

      <section className="settings-section">
        <h3>New downloads</h3>
        <label className="settings-toggle">
          <input type="checkbox" checked={defaultHot} onChange={toggleDefaultHot} />
          Mark newly pulled models Hot by default
        </label>
        <p className="settings-hint">
          Local UI preference only, applied when you pull a model from the Models tab.
        </p>
      </section>
    </div>
  );
}
