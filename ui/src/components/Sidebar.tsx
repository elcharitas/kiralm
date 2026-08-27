import { useEffect, useState } from "react";
import {
  BarChart3,
  ChevronRight,
  Filter,
  Folder,
  FolderPlus,
  FoldVertical,
  KeyRound,
  MoreHorizontal,
  PanelLeftClose,
  PanelLeftOpen,
  Radio,
  RotateCw,
  Search,
  Settings,
  Sparkles,
  SquarePen,
  Trash2,
  UnfoldVertical,
} from "lucide-react";
import { api, type Project } from "../api";
import { formatDuration, formatRelativeTime } from "../format";
import { previewOf, type ChatSession } from "../sessions";
import { useDialogs } from "./Dialogs";
import { ContextMenu } from "./ContextMenu";
import { useSettings } from "../settings";

const RECENT_LIMIT = 6;
const ALL_PROJECTS = "all";

interface ProjectGroup {
  project: Project;
  sessions: ChatSession[]; // sorted newest-first
}

export function Sidebar({
  sessions,
  projects,
  selectedProjectId,
  onCreateProject,
  onRefreshProjects,
  onDeleteProject,
  onRenameSession,
  onDeleteSession,
  activeId,
  streamingId,
  streamingSince,
  runningSessionIds,
  onSelect,
  onNewChat,
  onOpenSettings,
  settingsActive,
  collapsed,
  onToggleCollapse,
}: {
  sessions: ChatSession[];
  projects: Project[];
  selectedProjectId: string;
  onCreateProject: (path: string) => void;
  onRefreshProjects: () => Promise<void>;
  onDeleteProject: (id: string) => void;
  onRenameSession: (id: string, title: string) => void;
  onDeleteSession: (id: string) => void;
  activeId: string | null;
  streamingId: string | null;
  streamingSince: number | null;
  // Real server-side state (from the daemon), covers sessions this tab
  // didn't itself start streaming -- `streamingId` alone is just this
  // tab's own optimistic hint for its own send.
  runningSessionIds: Set<string>;
  onSelect: (id: string) => void;
  onNewChat: () => void;
  onOpenSettings: () => void;
  settingsActive: boolean;
  collapsed: boolean;
  onToggleCollapse: () => void;
}) {
  const [query, setQuery] = useState("");
  const [openSettled, setOpenSettled] = useState<Record<string, boolean>>({});
  const [refreshing, setRefreshing] = useState(false);
  const [, forceTick] = useState(0);
  const [sessionMenu, setSessionMenu] = useState<{ x: number; y: number; session: ChatSession } | null>(null);
  const { prompt, confirm, toast, browseFolder } = useDialogs();
  const { settings, save: saveSettings } = useSettings();
  const inProjectContext = activeId != null;

  // Persisted server-side (settings.json) so a collapsed workspace
  // stays collapsed across a reload, not just this tab's lifetime.
  const collapsedGroups: Record<string, boolean> = Object.fromEntries(
    settings.collapsed_workspaces.map((id) => [id, true]),
  );
  const setCollapsedGroups = (next: Record<string, boolean>) => {
    const collapsed_workspaces = Object.entries(next)
      .filter(([, v]) => v)
      .map(([id]) => id);
    saveSettings({ ...settings, collapsed_workspaces });
  };

  const [tunnelEnabled, setTunnelEnabled] = useState(false);

  useEffect(() => {
    api.tunnelStatus().then((s) => setTunnelEnabled(s.enabled), () => {});
  }, []);

  const toggleTunnel = async () => {
    try {
      if (tunnelEnabled) {
        await api.tunnelDisable();
        setTunnelEnabled(false);
        toast("Remote access disabled", "info");
      } else {
        await api.tunnelEnable();
        setTunnelEnabled(true);
        toast("Remote access enabled", "success");
      }
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const refresh = async () => {
    setRefreshing(true);
    try {
      await onRefreshProjects();
    } finally {
      setRefreshing(false);
    }
  };

  // Re-render once a second while a session is streaming so the
  // "Working Xm" badge reflects real elapsed time, not a snapshot.
  useEffect(() => {
    if (!streamingId) return;
    const id = window.setInterval(() => forceTick((n) => n + 1), 1000);
    return () => window.clearInterval(id);
  }, [streamingId]);

  const filtered = query.trim()
    ? sessions.filter((s) => s.title.toLowerCase().includes(query.trim().toLowerCase()))
    : sessions;

  // Real grouping: one header per registered project (even one with no
  // chats yet -- a freshly added workspace should show up immediately),
  // sessions nested under the project they're actually scoped to,
  // sorted by which project has the most recently updated session.
  const byProject = new Map<string, ChatSession[]>();
  for (const s of filtered) {
    const list = byProject.get(s.projectId);
    if (list) list.push(s);
    else byProject.set(s.projectId, [s]);
  }
  const groups: ProjectGroup[] = projects
    .filter((p) => selectedProjectId === ALL_PROJECTS || p.id === selectedProjectId)
    .map((project) => ({
      project,
      sessions: (byProject.get(project.id) ?? []).slice().sort((a, b) => b.updatedAt.localeCompare(a.updatedAt)),
    }))
    .sort((a, b) => {
      const aKey = a.sessions[0]?.updatedAt ?? a.project.last_active_at;
      const bKey = b.sessions[0]?.updatedAt ?? b.project.last_active_at;
      return bKey.localeCompare(aKey);
    });

  // One toggle, two directions: collapses every workspace's chat list
  // on the first click, expands them all back on the next.
  // Derived from the persisted per-workspace state, not tracked
  // separately, so it can't drift when groups are collapsed one by one.
  const allExpanded = groups.every((g) => !collapsedGroups[g.project.id]);

  const toggleAllGroups = () => {
    if (allExpanded) {
      setCollapsedGroups(Object.fromEntries(groups.map((g) => [g.project.id, true])));
    } else {
      setCollapsedGroups({});
    }
  };

  const renderRow = (s: ChatSession) => {
    const isActive = s.id === activeId && !settingsActive;
    return (
      <li key={s.id}>
        <button
          type="button"
          className={isActive ? "session-row session-row-active" : "session-row"}
          onClick={() => onSelect(s.id)}
          onContextMenu={(e) => {
            e.preventDefault();
            setSessionMenu({ x: e.clientX, y: e.clientY, session: s });
          }}
        >
          <div className="session-row-top">
            <span className="session-name truncate">{s.title}</span>
            <span className="session-time">{formatRelativeTime(s.updatedAt)}</span>
          </div>
          <div className="session-row-bottom">
            <span className="session-preview truncate">{previewOf(s)}</span>
            <span className="session-row-flare">
              <Sparkles size={12} />
            </span>
          </div>
        </button>
      </li>
    );
  };

  const addProject = async () => {
    const path = await browseFolder({ title: "Add workspace" });
    if (path && path.trim()) onCreateProject(path.trim());
  };

  const renameFromMenu = async (s: ChatSession) => {
    const title = await prompt({ title: "Rename chat", defaultValue: s.title });
    if (title && title.trim()) onRenameSession(s.id, title.trim());
  };

  const deleteFromMenu = async (s: ChatSession) => {
    const ok = await confirm({
      title: "Delete chat",
      message: `Delete "${s.title}"? This can't be undone.`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (ok) {
      onDeleteSession(s.id);
      toast("Chat deleted", "info");
    }
  };

  const deleteProjectFromMenu = async (project: Project) => {
    const ok = await confirm({
      title: "Delete workspace",
      message: `Delete "${project.name}"? Its chats go with it. The folder on disk is untouched.`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (ok) {
      onDeleteProject(project.id);
      toast("Workspace deleted", "info");
    }
  };

  // Collapsed = a slim icon rail (no search/session list) rather than
  // trying to cram that text-heavy content into a narrow column.
  if (collapsed) {
    return (
      <nav className="sidebar sidebar-collapsed">
        <button type="button" className="sidebar-footer-btn" title="Expand sidebar" onClick={onToggleCollapse}>
          <PanelLeftOpen size={16} />
        </button>
        <button type="button" className="sidebar-footer-btn" title="Add workspace" onClick={addProject}>
          <FolderPlus size={16} />
        </button>
        <div className="sidebar-collapsed-spacer" />
        <button
          type="button"
          className={settingsActive ? "sidebar-footer-btn sidebar-settings-btn-active" : "sidebar-footer-btn"}
          title="Settings"
          onClick={onOpenSettings}
        >
          <Settings size={16} />
        </button>
        <button
          type="button"
          className={tunnelEnabled ? "sidebar-footer-btn sidebar-tunnel-btn-active" : "sidebar-footer-btn"}
          title={tunnelEnabled ? "Remote access enabled -- click to disable" : "Enable remote access"}
          onClick={toggleTunnel}
        >
          <Radio size={16} />
        </button>
      </nav>
    );
  }

  return (
    <nav className="sidebar">
      <div className="sidebar-top-row">
        <div className="sidebar-search">
          <span className="sidebar-search-icon">
            <Search size={14} />
          </span>
          <input
            className="sidebar-search-input"
            placeholder="Search through workspaces"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>
        <button type="button" className="sidebar-collapse-btn" title="Collapse sidebar" onClick={onToggleCollapse}>
          <PanelLeftClose size={14} />
        </button>
      </div>

      <div className="sidebar-projects-row">
        <div className="sidebar-row-actions">
          <button type="button" className="sidebar-new-btn" title="Filter workspaces">
            <Filter size={14} />
          </button>
          <button type="button" className="sidebar-new-btn" title="Refresh" disabled={refreshing} onClick={refresh}>
            <RotateCw size={14} className={refreshing ? "spin-icon" : undefined} />
          </button>
          <button
            type="button"
            className="sidebar-new-btn"
            title={allExpanded ? "Collapse all workspaces" : "Expand all workspaces"}
            onClick={toggleAllGroups}
          >
            {allExpanded ? <FoldVertical size={14} /> : <UnfoldVertical size={14} />}
          </button>
          <button
            type="button"
            className="sidebar-new-btn"
            title={inProjectContext ? "New chat" : "Open a workspace first"}
            disabled={!inProjectContext}
            onClick={onNewChat}
          >
            <SquarePen size={14} />
          </button>
          <button type="button" className="sidebar-new-btn" title="Add workspace" onClick={addProject}>
            <FolderPlus size={14} />
          </button>
        </div>
        <button type="button" className="sidebar-new-btn" title="More options" disabled>
          <MoreHorizontal size={14} />
        </button>
      </div>

      <ul className="session-list">
        {groups.map(({ project, sessions: groupSessions }) => {
          const recent = groupSessions.slice(0, RECENT_LIMIT);
          const older = groupSessions.slice(RECENT_LIMIT);
          const isStreamingHere =
            (streamingId && streamingSince && groupSessions.some((s) => s.id === streamingId)) ||
            groupSessions.some((s) => runningSessionIds.has(s.id));
          const settled = openSettled[project.id] ?? false;
          const groupCollapsed = collapsedGroups[project.id] ?? false;
          return (
            <li key={project.id} className="session-project-group">
              <div className="session-project-header">
                <button
                  type="button"
                  className="session-project-collapse-btn"
                  title={groupCollapsed ? "Expand workspace" : "Collapse workspace"}
                  onClick={() => setCollapsedGroups({ ...collapsedGroups, [project.id]: !groupCollapsed })}
                >
                  <ChevronRight
                    size={12}
                    className={groupCollapsed ? "session-group-chevron" : "session-group-chevron session-group-chevron-open"}
                  />
                </button>
                <span className="sidebar-workspace-icon">
                  <Folder size={14} />
                </span>
                <span className="session-project-name">{project.name}</span>
                {isStreamingHere && (
                  <span className="session-working-badge">
                    <span className="session-dot session-dot-working" /> Working
                    {streamingSince ? ` ${formatDuration(Date.now() - streamingSince)}` : ""}
                  </span>
                )}
                <button
                  type="button"
                  className="session-project-menu-btn"
                  title="Delete workspace"
                  onClick={(e) => {
                    e.stopPropagation();
                    deleteProjectFromMenu(project);
                  }}
                >
                  <Trash2 size={12} />
                </button>
              </div>
              {!groupCollapsed && (
                <>
                  <ul className="session-list session-project-rows">{recent.map(renderRow)}</ul>
                  {older.length > 0 && (
                    <>
                      <button
                        type="button"
                        className="session-group-toggle"
                        onClick={() => setOpenSettled((prev) => ({ ...prev, [project.id]: !settled }))}
                      >
                        <span className={settled ? "session-group-chevron session-group-chevron-open" : "session-group-chevron"}>
                          <ChevronRight size={12} />
                        </span>
                        Settled ({older.length})
                      </button>
                      {settled && <ul className="session-list session-project-rows">{older.map(renderRow)}</ul>}
                    </>
                  )}
                </>
              )}
            </li>
          );
        })}
        {groups.length === 0 && (
          <li className="session-list-empty">{sessions.length === 0 ? "No chats yet" : "No matches"}</li>
        )}
      </ul>

      <div className="sidebar-footer">
        <div className="sidebar-footer-icons">
          <button
            type="button"
            className={settingsActive ? "sidebar-footer-btn sidebar-settings-btn-active" : "sidebar-footer-btn"}
            title="Settings"
            onClick={onOpenSettings}
          >
            <Settings size={16} />
          </button>
          <button type="button" className="sidebar-footer-btn" title="Keys">
            <KeyRound size={16} />
          </button>
          <button type="button" className="sidebar-footer-btn" title="Usage">
            <BarChart3 size={16} />
          </button>
        </div>
        <button
          type="button"
          className={tunnelEnabled ? "sidebar-footer-btn sidebar-tunnel-btn-active" : "sidebar-footer-btn"}
          title={tunnelEnabled ? "Remote access enabled -- click to disable" : "Enable remote access"}
          onClick={toggleTunnel}
        >
          <Radio size={16} />
        </button>
      </div>

      {sessionMenu && (
        <ContextMenu
          x={sessionMenu.x}
          y={sessionMenu.y}
          onClose={() => setSessionMenu(null)}
          items={[
            { label: "Rename", onClick: () => renameFromMenu(sessionMenu.session) },
            { label: "Delete", danger: true, onClick: () => deleteFromMenu(sessionMenu.session) },
          ]}
        />
      )}
    </nav>
  );
}
