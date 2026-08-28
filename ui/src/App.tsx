import { useEffect, useRef, useState } from "react";
import { Sidebar } from "./components/Sidebar";
import { TopBar } from "./components/TopBar";
import { RightPanel } from "./components/RightPanel";
import { AppLogo } from "./components/AppLogo";
import { ChatView } from "./views/ChatView";
import { HomeView } from "./views/HomeView";
import { SettingsView, type SettingsTab } from "./views/SettingsView";
import { useStatus } from "./hooks/useStatus";
import { useSessions } from "./hooks/useSessions";
import { useProjects } from "./hooks/useProjects";
import { useRunningGenerations } from "./hooks/useRunningGenerations";
import { useGitStatus } from "./hooks/useGitStatus";
import { useSettings } from "./settings";
import { useDialogs } from "./components/Dialogs";
import type { Project } from "./api";

type View = "thread" | "settings" | "home";

export default function App() {
  const [view, setView] = useState<View>("thread");
  const [settingsTab, setSettingsTab] = useState<SettingsTab>("general");
  const [streamingId, setStreamingId] = useState<string | null>(null);
  const [streamingSince, setStreamingSince] = useState<number | null>(null);
  const [selectedProjectId, setSelectedProjectId] = useState("all");
  const [rightPanelMode, setRightPanelMode] = useState<"split" | "collapsed">("split");
  const { settings, save: saveSettings } = useSettings();
  const { toast } = useDialogs();
  // Persisted server-side (settings.json), not local state -- collapse
  // should survive a reload/relaunch, not just this tab's lifetime.
  const sidebarCollapsed = settings.sidebar_collapsed;
  const setSidebarCollapsed = (next: boolean) =>
    saveSettings({ ...settings, sidebar_collapsed: next }).catch((e) =>
      toast(e instanceof Error ? e.message : String(e), "error"),
    );
  const { status, error, refresh } = useStatus();
  const {
    sessions,
    active,
    activeId,
    setActiveId,
    createSession,
    setMessages,
    setPermission,
    renameSession,
    deleteSession,
    deleteSessionsByProject,
  } = useSessions();
  const { projects, createProject, cloneProject, deleteProject, refresh: refreshProjects } = useProjects();
  const runningGenerations = useRunningGenerations();
  const { status: gitStatus } = useGitStatus(active?.projectId);

  // No chat selected (fresh launch, or the active one got cleared) and
  // not in settings -> the project-home screen, not an empty ChatView.
  const effectiveView: View = view === "thread" && !active ? "home" : view;

  // Fires a desktop notification for any session that just dropped out
  // of the running set (i.e. finished) while it wasn't the one focused
  // -- the point is surfacing background work you weren't watching.
  const prevRunningRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    const prev = prevRunningRef.current;
    if (settings.notify_on_finish && typeof Notification !== "undefined" && Notification.permission === "granted") {
      for (const id of prev) {
        if (runningGenerations.has(id) || id === activeId) continue;
        const finished = sessions.find((s) => s.id === id);
        new Notification("KiraLM", { body: `"${finished?.title ?? "A chat"}" finished generating.` });
      }
    }
    prevRunningRef.current = runningGenerations;
  }, [runningGenerations, activeId, sessions, settings.notify_on_finish]);

  // New chats land in whichever project the currently active session
  // belongs to (i.e. whatever workspace you're actually looking at).
  // Only ever called while a real session is active -- Sidebar's "New
  // chat" is disabled otherwise, and ChatView (the only other caller,
  // via onEnsureSession) doesn't mount without one.
  const newChat = () => createSession(active!.projectId);

  const openProject = (project: Project) => {
    setSelectedProjectId(project.id);
    createSession(project.id).catch(() => {});
    setView("thread");
  };

  // Its chats go with it -- an orphaned session pointing at a deleted
  // project would just 404 on every subsequent git-status/touch call.
  const removeProject = (id: string) => {
    deleteProject(id);
    deleteSessionsByProject(id);
    setSelectedProjectId((cur) => (cur === id ? "all" : cur));
  };

  // Tracks when the current stream started, so the sidebar can show a
  // real "Working Xm" elapsed badge instead of a static spinner only.
  useEffect(() => {
    setStreamingSince(streamingId ? Date.now() : null);
  }, [streamingId]);

  const openSettings = (tab: SettingsTab = "general") => {
    setSettingsTab(tab);
    setView("settings");
  };

  // The three floating panels resize by shifting the grid's column
  // sizes directly -- fewer combinatorial CSS classes than one per
  // (sidebar x right-panel) state pairing.
  // Right panel is a fixed width, not a fraction, so it stays put
  // instead of ballooning on a wide window -- only the middle (chat)
  // column should actually get the extra space.
  const gridTemplateColumns = [
    sidebarCollapsed ? "56px" : "260px",
    "1fr",
    rightPanelMode === "collapsed" ? "56px" : "340px",
  ].join(" ");

  return (
    <div className="app" style={{ gridTemplateColumns }} data-tauri-drag-region>
      <div className="desktop-title-brand" data-tauri-drag-region>
        <AppLogo className="desktop-title-logo" />
        <span>KiraLM</span>
      </div>
      <Sidebar
        sessions={sessions}
        projects={projects}
        selectedProjectId={selectedProjectId}
        onCreateProject={(path) => createProject(path).then(openProject)}
        onRefreshProjects={refreshProjects}
        onDeleteProject={removeProject}
        onRenameSession={renameSession}
        onDeleteSession={deleteSession}
        activeId={activeId}
        streamingId={streamingId}
        streamingSince={streamingSince}
        runningSessionIds={runningGenerations}
        collapsed={sidebarCollapsed}
        onToggleCollapse={() => setSidebarCollapsed(!sidebarCollapsed)}
        settingsActive={view === "settings"}
        onSelect={(id) => {
          setActiveId(id);
          setView("thread");
        }}
        onNewChat={() => {
          newChat();
          setView("thread");
        }}
        onOpenSettings={() => openSettings("general")}
      />

      {effectiveView !== "home" && (
        <TopBar
          itemName={effectiveView === "settings" ? "Settings" : active?.title ?? "New chat"}
          projectName={effectiveView === "thread" ? projects.find((p) => p.id === active?.projectId)?.name ?? null : null}
          onClose={() => {
            setActiveId(null);
            setView("thread");
          }}
        />
      )}

      <main className={effectiveView === "home" ? "main-body main-body-full" : "main-body"}>
        {error && <div className="banner banner-bad app-conn-error">{error}</div>}
        {effectiveView === "settings" ? (
          <SettingsView status={status} refresh={refresh} tab={settingsTab} onTabChange={setSettingsTab} />
        ) : effectiveView === "home" ? (
          <HomeView onCreateProject={createProject} onCloneProject={cloneProject} onOpenProject={openProject} />
        ) : (
          <ChatView
            status={status}
            session={active}
            gitStatus={gitStatus}
            onUpdateMessages={setMessages}
            onEnsureSession={newChat}
            onStreamingChange={setStreamingId}
            onTogglePermission={(id) =>
              setPermission(id, sessions.find((s) => s.id === id)?.permission === "full-access" ? "read-only" : "full-access")
            }
          />
        )}
      </main>

      <RightPanel
        status={status}
        session={effectiveView === "thread" ? active : null}
        mode={rightPanelMode}
        onModeChange={setRightPanelMode}
      />
    </div>
  );
}
