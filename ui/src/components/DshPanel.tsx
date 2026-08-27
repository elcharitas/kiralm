import { useEffect, useState } from "react";
import { Plus, Trash2, X } from "lucide-react";
import { api, type DshStatus } from "../api";
import { useDialogs } from "./Dialogs";

// DeepSeek Harness (dsh) bridge: detection status (not-installed is
// the normal, expected state for this developer-preview tool, not an
// error) plus profile list, drilling into one for its plugin list --
// same list/drill-down shape as MarketplacePanel.
export function DshPanel() {
  const [status, setStatus] = useState<DshStatus | null>(null);
  const [profiles, setProfiles] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [plugins, setPlugins] = useState<string[]>([]);
  const [pluginsLoading, setPluginsLoading] = useState(false);
  const { toast, confirm } = useDialogs();

  const loadStatus = () => api.dshStatus().then(setStatus, () => {});
  const load = () => api.listDshProfiles().then((d) => setProfiles(d.profiles), () => {});

  useEffect(() => {
    Promise.all([loadStatus(), load()]).finally(() => setLoading(false));
  }, []);

  const loadPlugins = (profile: string) => {
    setPluginsLoading(true);
    api
      .listDshPlugins(profile)
      .then((d) => setPlugins(d.plugins), () => {})
      .finally(() => setPluginsLoading(false));
  };

  const open = (profile: string) => {
    setSelected(profile);
    loadPlugins(profile);
  };

  const create = async () => {
    if (!name.trim()) return;
    try {
      await api.createDshProfile(name.trim());
      setName("");
      setAdding(false);
      await load();
      toast("Profile created", "success");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const remove = async (profile: string) => {
    const ok = await confirm({
      title: "Delete profile",
      message: `Delete "${profile}"? This can't be undone.`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteDshProfile(profile);
      if (selected === profile) {
        setSelected(null);
        setPlugins([]);
      }
      await load();
      toast("Profile deleted", "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const removePlugin = async (name: string) => {
    if (!selected) return;
    try {
      const result = await api.removeDshPlugin(selected, name);
      setPlugins(result.plugins);
      toast(`${name} removed`, "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  if (selected) {
    return (
      <div className="skills-panel">
        <div className="skills-viewer-head">
          <button type="button" className="right-panel-icon-btn" title="Back" onClick={() => setSelected(null)}>
            <X size={14} />
          </button>
          <span className="mono truncate">{selected}</span>
        </div>

        {/* Installing by package name moved to the Plugins tab (dsh
            source kind); this view stays read-only-plus-remove so a
            stray install can still be cleaned up per profile. */}
        {pluginsLoading ? (
          <p className="settings-hint">Loading…</p>
        ) : plugins.length === 0 ? (
          <p className="settings-hint">No plugins installed in this profile.</p>
        ) : (
          <ul className="skills-list">
            {plugins.map((p) => (
              <li key={p} className="skills-row">
                <div className="skills-row-main">
                  <span className="mono truncate">{p}</span>
                </div>
                <button type="button" className="right-panel-icon-btn" title="Remove" onClick={() => removePlugin(p)}>
                  <Trash2 size={14} />
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    );
  }

  return (
    <div className="skills-panel">
      {loading ? (
        <p className="settings-hint">Loading…</p>
      ) : (
        <p className="settings-hint">
          {status?.detected
            ? `dsh detected${status.version ? ` (${status.version})` : ""}`
            : "dsh not detected -- install DeepSeek Harness (developer preview) and ensure it's on PATH."}
        </p>
      )}

      <div className="skills-panel-actions">
        <button type="button" className="btn" onClick={() => setAdding((v) => !v)}>
          <Plus size={14} /> New profile
        </button>
      </div>

      {adding && (
        <div className="skills-add-form">
          <input
            autoFocus
            className="mcp-field mono"
            placeholder="profile name"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <div className="modal-actions">
            <button type="button" className="btn" onClick={() => setAdding(false)}>
              Cancel
            </button>
            <button type="button" className="btn btn-accent" disabled={!name.trim()} onClick={create}>
              Create
            </button>
          </div>
        </div>
      )}

      {!loading &&
        (profiles.length === 0 ? (
          <p className="settings-hint">No dsh profiles configured yet.</p>
        ) : (
          <ul className="skills-list">
            {profiles.map((p) => (
              <li key={p} className="skills-row">
                <button type="button" className="skills-row-main" onClick={() => open(p)}>
                  <span className="mono truncate">{p}</span>
                </button>
                <button type="button" className="right-panel-icon-btn" title="Delete" onClick={() => remove(p)}>
                  <Trash2 size={14} />
                </button>
              </li>
            ))}
          </ul>
        ))}
    </div>
  );
}
