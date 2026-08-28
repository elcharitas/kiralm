import { useEffect, useState } from "react";
import { Plus, Trash2, RefreshCw } from "lucide-react";
import { api, type McpServer } from "../api";
import { useDialogs } from "./Dialogs";

// Manual MCP server registration + live enable/disable/refresh/delete.
// Global, no per-workspace scoping, same posture as SkillsPanel.
export function McpPanel() {
  const [servers, setServers] = useState<McpServer[]>([]);
  const [loading, setLoading] = useState(true);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [env, setEnv] = useState("");
  const { toast, confirm } = useDialogs();

  const load = () =>
    api.listMcpServers().then(
      (d) => setServers(d.servers),
      () => {},
    );

  useEffect(() => {
    load().finally(() => setLoading(false));
  }, []);

  const parseEnv = (raw: string): Record<string, string> => {
    const out: Record<string, string> = {};
    for (const line of raw.split("\n")) {
      const eq = line.indexOf("=");
      if (eq <= 0) continue;
      out[line.slice(0, eq).trim()] = line.slice(eq + 1).trim();
    }
    return out;
  };

  const add = async () => {
    if (!name.trim() || !command.trim()) return;
    try {
      await api.addMcpServer({
        name: name.trim(),
        command: command.trim(),
        args: args.trim() ? args.trim().split(/\s+/) : [],
        env: parseEnv(env),
      });
      setName("");
      setCommand("");
      setArgs("");
      setEnv("");
      setAdding(false);
      await load();
      toast("MCP server added", "success");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const toggle = async (server: McpServer) => {
    const next = !server.enabled;
    setServers((prev) => prev.map((s) => (s.name === server.name ? { ...s, enabled: next } : s)));
    try {
      await api.setMcpServerEnabled(server.name, next);
      await load();
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
      await load();
    }
  };

  const refresh = async (server: McpServer) => {
    try {
      await api.refreshMcpServer(server.name);
      await load();
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const remove = async (server: McpServer) => {
    const ok = await confirm({
      title: "Delete MCP server",
      message: `Delete "${server.name}"? This can't be undone.`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteMcpServer(server.name);
      await load();
      toast("MCP server deleted", "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  return (
    <div className="skills-panel">
      <div className="skills-panel-actions">
        <button type="button" className="btn" onClick={() => setAdding((v) => !v)}>
          <Plus size={14} />Add server
        </button>
      </div>

      {adding && (
        <div className="skills-add-form">
          <input
            autoFocus
            className="mcp-field mono"
            placeholder="name (e.g. everything)"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <input
            className="mcp-field mono"
            placeholder="command (e.g. npx)"
            value={command}
            onChange={(e) => setCommand(e.target.value)}
          />
          <input
            className="mcp-field mono"
            placeholder="args, space-separated (e.g. -y @modelcontextprotocol/server-everything)"
            value={args}
            onChange={(e) => setArgs(e.target.value)}
          />
          <textarea
            className="skills-paste-input mono"
            placeholder={"env, one KEY=VALUE per line"}
            value={env}
            onChange={(e) => setEnv(e.target.value)}
          />
          <div className="modal-actions">
            <button type="button" className="btn" onClick={() => setAdding(false)}>
              Cancel
            </button>
            <button
              type="button"
              className="btn btn-accent"
              disabled={!name.trim() || !command.trim()}
              onClick={add}
            >
              Add server
            </button>
          </div>
        </div>
      )}

      {loading ? (
        <p className="settings-hint">Loading…</p>
      ) : servers.length === 0 ? (
        <p className="settings-hint">No MCP servers configured yet.</p>
      ) : (
        <ul className="skills-list">
          {servers.map((s) => (
            <li key={s.name} className="skills-row">
              <div className="skills-row-main">
                <span className="mono truncate">{s.name}</span>
                <span className="settings-hint skills-row-desc truncate">
                  {s.command} {s.args.join(" ")}
                </span>
                {s.enabled && (
                  <span className={s.connected ? "mcp-pill mcp-pill-ok" : "mcp-pill mcp-pill-error"}>
                    {s.connected ? `connected · ${s.tool_count} tools` : `failed${s.error ? `: ${s.error}` : ""}`}
                  </span>
                )}
              </div>
              <button
                type="button"
                className="right-panel-icon-btn"
                title="Refresh tools"
                disabled={!s.enabled}
                onClick={() => refresh(s)}
              >
                <RefreshCw size={14} />
              </button>
              <label className="switch-input-wrap" onClick={(e) => e.stopPropagation()}>
                <input type="checkbox" className="switch-input" checked={s.enabled} onChange={() => toggle(s)} />
              </label>
              <button type="button" className="right-panel-icon-btn" title="Delete" onClick={() => remove(s)}>
                <Trash2 size={14} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
