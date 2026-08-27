import { useEffect, useState } from "react";
import { Plus, RefreshCw, Trash2, X } from "lucide-react";
import { api, type Marketplace, type MarketplacePlugin } from "../api";
import { useDialogs } from "./Dialogs";

// Add-by-url + list marketplaces, drill into one to install/uninstall
// its plugins. Global, same posture as SkillsPanel/McpPanel -- what
// each panel writes into (skills.rs/mcp.rs's stores) lives there too.
export function MarketplacePanel() {
  const [marketplaces, setMarketplaces] = useState<Marketplace[]>([]);
  const [loading, setLoading] = useState(true);
  const [adding, setAdding] = useState(false);
  const [url, setUrl] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [plugins, setPlugins] = useState<MarketplacePlugin[]>([]);
  const [pluginsLoading, setPluginsLoading] = useState(false);
  const { toast, confirm } = useDialogs();

  const load = () => api.listMarketplaces().then((d) => setMarketplaces(d.marketplaces), () => {});

  useEffect(() => {
    load().finally(() => setLoading(false));
  }, []);

  const loadPlugins = (slug: string) => {
    setPluginsLoading(true);
    api
      .listMarketplacePlugins(slug)
      .then((d) => setPlugins(d.plugins), () => {})
      .finally(() => setPluginsLoading(false));
  };

  const open = (slug: string) => {
    setSelected(slug);
    loadPlugins(slug);
  };

  const add = async () => {
    if (!url.trim()) return;
    try {
      const m = await api.addMarketplace(url.trim());
      setUrl("");
      setAdding(false);
      await load();
      toast("Marketplace added", "success");
      open(m.slug);
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const refresh = async (slug: string) => {
    try {
      await api.refreshMarketplace(slug);
      await load();
      if (selected === slug) loadPlugins(slug);
      toast("Marketplace refreshed", "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const remove = async (slug: string) => {
    const ok = await confirm({
      title: "Remove marketplace",
      message: `Remove "${slug}"? Already-installed plugins stay installed.`,
      confirmLabel: "Remove",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.removeMarketplace(slug);
      if (selected === slug) {
        setSelected(null);
        setPlugins([]);
      }
      await load();
      toast("Marketplace removed", "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const install = async (plugin: MarketplacePlugin) => {
    if (!selected) return;
    try {
      const result = await api.installPlugin(selected, plugin.name);
      loadPlugins(selected);
      toast(`${plugin.name} installed (${result.skills.length} skill(s), ${result.mcp_servers.length} mcp server(s))`, "success");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const uninstall = async (plugin: MarketplacePlugin) => {
    if (!selected) return;
    try {
      await api.uninstallPlugin(selected, plugin.name);
      loadPlugins(selected);
      toast(`${plugin.name} uninstalled`, "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  if (selected) {
    const marketplace = marketplaces.find((m) => m.slug === selected);
    return (
      <div className="skills-panel">
        <div className="skills-viewer-head">
          <button type="button" className="right-panel-icon-btn" title="Back" onClick={() => setSelected(null)}>
            <X size={14} />
          </button>
          <span className="mono truncate">{marketplace?.name ?? selected}</span>
          <button
            type="button"
            className="right-panel-icon-btn"
            title="Refresh"
            onClick={() => refresh(selected)}
          >
            <RefreshCw size={14} />
          </button>
        </div>
        {pluginsLoading ? (
          <p className="settings-hint">Loading…</p>
        ) : plugins.length === 0 ? (
          <p className="settings-hint">No plugins in this marketplace.</p>
        ) : (
          <ul className="skills-list">
            {plugins.map((p) => (
              <li key={p.name} className="skills-row">
                <div className="skills-row-main">
                  <span className="mono truncate">{p.name}</span>
                  <span className="settings-hint skills-row-desc truncate">{p.description}</span>
                </div>
                {p.installed ? (
                  <button type="button" className="btn" onClick={() => uninstall(p)}>
                    Uninstall
                  </button>
                ) : (
                  <button type="button" className="btn btn-accent" onClick={() => install(p)}>
                    Install
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
    );
  }

  return (
    <div className="skills-panel">
      <div className="skills-panel-actions">
        <button type="button" className="btn" onClick={() => setAdding((v) => !v)}>
          <Plus size={14} /> Add marketplace
        </button>
      </div>

      {adding && (
        <div className="skills-add-form">
          <input
            autoFocus
            className="mcp-field mono"
            placeholder="git url (e.g. https://github.com/org/repo)"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
          />
          <div className="modal-actions">
            <button type="button" className="btn" onClick={() => setAdding(false)}>
              Cancel
            </button>
            <button type="button" className="btn btn-accent" disabled={!url.trim()} onClick={add}>
              Add
            </button>
          </div>
        </div>
      )}

      {loading ? (
        <p className="settings-hint">Loading…</p>
      ) : marketplaces.length === 0 ? (
        <p className="settings-hint">No marketplaces added yet.</p>
      ) : (
        <ul className="skills-list">
          {marketplaces.map((m) => (
            <li key={m.slug} className="skills-row">
              <button type="button" className="skills-row-main" onClick={() => open(m.slug)}>
                <span className="mono truncate">{m.name}</span>
                <span className="settings-hint skills-row-desc truncate">
                  {m.plugins.length} plugin{m.plugins.length === 1 ? "" : "s"}
                </span>
              </button>
              <button type="button" className="right-panel-icon-btn" title="Refresh" onClick={() => refresh(m.slug)}>
                <RefreshCw size={14} />
              </button>
              <button type="button" className="right-panel-icon-btn" title="Remove" onClick={() => remove(m.slug)}>
                <Trash2 size={14} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
