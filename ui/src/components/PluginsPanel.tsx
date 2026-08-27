import { useEffect, useState } from "react";
import { api, type CatalogPlugin } from "../api";
import { useDialogs } from "./Dialogs";

const sourceKindLabel: Record<CatalogPlugin["source_kind"], string> = {
  claude_marketplace: "Claude Marketplace",
  agent_plugin: "Agent Plugin",
  dsh: "dsh",
};

const sourceKindBadgeClass: Record<CatalogPlugin["source_kind"], string> = {
  claude_marketplace: "badge-good",
  agent_plugin: "badge-warn",
  dsh: "badge-bad",
};

// Single browsable catalog, sourced from kiralm-web (GET /api/plugins),
// covering all three install backends (marketplace.rs, agent_plugin.rs,
// dsh.rs) behind one Install/Uninstall button each -- no URL/path/
// package text input anywhere here, the source is baked into the
// catalog row server-side.
export function PluginsPanel() {
  const [plugins, setPlugins] = useState<CatalogPlugin[]>([]);
  const [loading, setLoading] = useState(true);
  const [pendingId, setPendingId] = useState<string | null>(null);
  const { toast } = useDialogs();

  const load = () => api.listPluginCatalog().then((d) => setPlugins(d.data), () => {});

  useEffect(() => {
    load().finally(() => setLoading(false));
  }, []);

  const install = async (p: CatalogPlugin) => {
    setPendingId(p.id);
    try {
      await api.installCatalogPlugin(p.id);
      await load();
      toast(`${p.name} installed`, "success");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      setPendingId(null);
    }
  };

  const uninstall = async (p: CatalogPlugin) => {
    setPendingId(p.id);
    try {
      await api.uninstallCatalogPlugin(p.id);
      await load();
      toast(`${p.name} uninstalled`, "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      setPendingId(null);
    }
  };

  if (loading) {
    return (
      <div className="skills-panel">
        <p className="settings-hint">Loading…</p>
      </div>
    );
  }

  if (plugins.length === 0) {
    return (
      <div className="skills-panel">
        <p className="settings-hint">No plugins available yet.</p>
      </div>
    );
  }

  return (
    <div className="skills-panel">
      <ul className="skills-list">
        {plugins.map((p) => (
          <li key={p.id} className="skills-row">
            <div className="skills-row-main">
              <span className="mono truncate">{p.name}</span>
              <span className={`badge ${sourceKindBadgeClass[p.source_kind]}`}>
                {sourceKindLabel[p.source_kind]}
              </span>
              <span className="settings-hint skills-row-desc truncate">
                {p.description ?? p.source}
              </span>
            </div>
            {p.installed ? (
              <button type="button" className="btn" disabled={pendingId === p.id} onClick={() => uninstall(p)}>
                {pendingId === p.id ? "Uninstalling…" : "Uninstall"}
              </button>
            ) : (
              <button
                type="button"
                className="btn btn-accent"
                disabled={pendingId === p.id}
                onClick={() => install(p)}
              >
                {pendingId === p.id ? "Installing…" : "Install"}
              </button>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}
