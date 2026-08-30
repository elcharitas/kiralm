import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { Sidebar } from "./components/Sidebar";
import { TopBar } from "./components/TopBar";
import { RightPanel } from "./components/RightPanel";
import { RuntimeDock } from "./components/RuntimeDock";
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
import {
  COLLAPSED_ISLAND_WIDTH,
  COLLAPSE_THRESHOLD,
  DEFAULT_RIGHT_PANEL_WIDTH,
  DEFAULT_SIDEBAR_WIDTH,
  MAX_ISLAND_WIDTH,
  MIN_ISLAND_WIDTH,
  MIN_MAIN_WIDTH,
  SHELL_HORIZONTAL_SPACE,
  clamp,
  fitExpandedIslands,
} from "./islandLayout";
import type { Project } from "./api";

type View = "thread" | "settings" | "home";

const KEYBOARD_RESIZE_STEP = 16;

type IslandSide = "left" | "right";

export default function App() {
  const [view, setView] = useState<View>("thread");
  const [settingsTab, setSettingsTab] = useState<SettingsTab>("general");
  const [streamingId, setStreamingId] = useState<string | null>(null);
  const [streamingSince, setStreamingSince] = useState<number | null>(null);
  const [selectedProjectId, setSelectedProjectId] = useState("all");
  const [rightPanelMode, setRightPanelMode] = useState<"split" | "collapsed">("split");
  const [gitRefreshVersion, setGitRefreshVersion] = useState(0);
  const [sidebarWidth, setSidebarWidth] = useState(DEFAULT_SIDEBAR_WIDTH);
  const [rightPanelWidth, setRightPanelWidth] = useState(
    DEFAULT_RIGHT_PANEL_WIDTH,
  );
  const appRef = useRef<HTMLDivElement>(null);
  const sidebarWidthRef = useRef(DEFAULT_SIDEBAR_WIDTH);
  const rightPanelWidthRef = useRef(DEFAULT_RIGHT_PANEL_WIDTH);
  const islandResizeCleanupRef = useRef<(() => void) | null>(null);
  const { settings, save: saveSettings } = useSettings();
  const { toast } = useDialogs();
  // Persisted server-side (settings.json), not local state -- collapse
  // should survive a reload/relaunch, not just this tab's lifetime.
  const sidebarCollapsed = settings.sidebar_collapsed;
  const setSidebarCollapsed = useCallback(
    (next: boolean) =>
      saveSettings({ ...settings, sidebar_collapsed: next }).catch((e) =>
        toast(e instanceof Error ? e.message : String(e), "error"),
      ),
    [saveSettings, settings, toast],
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
    refreshSessions,
  } = useSessions();
  const { projects, createProject, cloneProject, deleteProject, refresh: refreshProjects } = useProjects();
  const runningGenerations = useRunningGenerations();
  const { status: gitStatus, refresh: refreshGitStatus } = useGitStatus(
    active?.projectId,
  );
  const refreshGit = useCallback(async () => {
    await refreshGitStatus();
    setGitRefreshVersion((version) => version + 1);
  }, [refreshGitStatus]);
  const activeProjectBusy = active
    ? sessions.some(
        (session) =>
          session.projectId === active.projectId &&
          runningGenerations.has(session.id),
      )
    : false;

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

  const updateIslandWidth = useCallback(
    (side: IslandSide, width: number) => {
      if (side === "left") {
        sidebarWidthRef.current = width;
        setSidebarWidth(width);
      } else {
        rightPanelWidthRef.current = width;
        setRightPanelWidth(width);
      }
    },
    [],
  );

  const collapseIsland = useCallback(
    (side: IslandSide) => {
      if (side === "left") void setSidebarCollapsed(true);
      else setRightPanelMode("collapsed");
    },
    [setSidebarCollapsed],
  );

  const availableIslandWidth = useCallback(
    (side: IslandSide, containerWidth: number) => {
      const oppositeWidth = side === "left"
        ? rightPanelMode === "collapsed"
          ? COLLAPSED_ISLAND_WIDTH
          : rightPanelWidthRef.current
        : sidebarCollapsed
          ? COLLAPSED_ISLAND_WIDTH
          : sidebarWidthRef.current;
      return Math.max(
        COLLAPSE_THRESHOLD + 1,
        containerWidth
          - SHELL_HORIZONTAL_SPACE
          - oppositeWidth
          - MIN_MAIN_WIDTH,
      );
    },
    [rightPanelMode, sidebarCollapsed],
  );

  useLayoutEffect(() => {
    const app = appRef.current;
    if (!app) return;
    const reclamp = () => {
      const [left, right] = fitExpandedIslands(
        app.getBoundingClientRect().width,
        sidebarWidthRef.current,
        rightPanelWidthRef.current,
        sidebarCollapsed,
        rightPanelMode === "collapsed",
      );
      if (left !== sidebarWidthRef.current) updateIslandWidth("left", left);
      if (right !== rightPanelWidthRef.current) {
        updateIslandWidth("right", right);
      }
    };
    reclamp();
    const observer = new ResizeObserver(reclamp);
    observer.observe(app);
    window.addEventListener("resize", reclamp);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", reclamp);
    };
  }, [rightPanelMode, sidebarCollapsed, updateIslandWidth]);

  useEffect(
    () => () => islandResizeCleanupRef.current?.(),
    [],
  );

  const beginIslandResize = (
    side: IslandSide,
    event: ReactPointerEvent<HTMLDivElement>,
  ) => {
    const app = appRef.current;
    if (!app || event.button !== 0 || !event.isPrimary) return;
    event.preventDefault();
    islandResizeCleanupRef.current?.();
    const rect = app.getBoundingClientRect();
    const target = event.currentTarget;
    const pointerId = event.pointerId;
    const initialWidth = side === "left"
      ? sidebarWidthRef.current
      : rightPanelWidthRef.current;
    let lastWidth = initialWidth;
    let finished = false;
    target.setPointerCapture(pointerId);
    document.body.classList.add("island-resizing");

    const finish = (snap = true) => {
      if (finished) return;
      finished = true;
      document.body.classList.remove("island-resizing");
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", onEnd);
      window.removeEventListener("pointercancel", onEnd);
      window.removeEventListener("blur", onBlur);
      target.removeEventListener("lostpointercapture", onLostCapture);
      if (target.hasPointerCapture(pointerId)) {
        target.releasePointerCapture(pointerId);
      }
      islandResizeCleanupRef.current = null;
      if (snap && lastWidth < MIN_ISLAND_WIDTH) {
        const maximum = availableIslandWidth(side, rect.width);
        updateIslandWidth(side, Math.min(MIN_ISLAND_WIDTH, maximum));
      }
    };

    const move = (pointerEvent: globalThis.PointerEvent) => {
      if (pointerEvent.pointerId !== pointerId) return;
      const proposed = side === "left"
        ? pointerEvent.clientX - rect.left - 12
        : rect.right - pointerEvent.clientX - 12;
      if (proposed <= COLLAPSE_THRESHOLD) {
        finish(false);
        updateIslandWidth(side, initialWidth);
        collapseIsland(side);
        return;
      }
      lastWidth = clamp(
        proposed,
        COLLAPSE_THRESHOLD + 1,
        Math.min(MAX_ISLAND_WIDTH, availableIslandWidth(side, rect.width)),
      );
      updateIslandWidth(side, lastWidth);
    };

    const onEnd = (pointerEvent: globalThis.PointerEvent) => {
      if (pointerEvent.pointerId === pointerId) finish();
    };
    const onBlur = () => finish();
    const onLostCapture = () => finish();

    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", onEnd);
    window.addEventListener("pointercancel", onEnd);
    window.addEventListener("blur", onBlur);
    target.addEventListener("lostpointercapture", onLostCapture);
    islandResizeCleanupRef.current = finish;
  };

  const resizeIslandWithKeyboard = (
    side: IslandSide,
    event: ReactKeyboardEvent<HTMLDivElement>,
  ) => {
    const app = appRef.current;
    if (!app) return;
    const shrinkingKey = side === "left" ? "ArrowLeft" : "ArrowRight";
    const growingKey = side === "left" ? "ArrowRight" : "ArrowLeft";
    if (event.key === "Home") {
      event.preventDefault();
      collapseIsland(side);
      return;
    }
    if (event.key !== shrinkingKey && event.key !== growingKey
      && event.key !== "End") return;
    event.preventDefault();
    const current = side === "left"
      ? sidebarWidthRef.current
      : rightPanelWidthRef.current;
    const maximum = Math.min(
      MAX_ISLAND_WIDTH,
      availableIslandWidth(side, app.getBoundingClientRect().width),
    );
    if (event.key === "End") {
      updateIslandWidth(side, maximum);
      return;
    }
    if (event.key === shrinkingKey && current <= MIN_ISLAND_WIDTH) {
      collapseIsland(side);
      return;
    }
    const delta = event.key === growingKey
      ? KEYBOARD_RESIZE_STEP
      : -KEYBOARD_RESIZE_STEP;
    updateIslandWidth(
      side,
      clamp(current + delta, MIN_ISLAND_WIDTH, maximum),
    );
  };

  // The three floating panels resize by shifting the grid's column
  // sizes directly -- fewer combinatorial CSS classes than one per
  // (sidebar x right-panel) state pairing.
  // Right panel is a fixed width, not a fraction, so it stays put
  // instead of ballooning on a wide window -- only the middle (chat)
  // column should actually get the extra space.
  const gridTemplateColumns = [
    sidebarCollapsed ? `${COLLAPSED_ISLAND_WIDTH}px` : `${sidebarWidth}px`,
    "1fr",
    rightPanelMode === "collapsed"
      ? `${COLLAPSED_ISLAND_WIDTH}px`
      : `${rightPanelWidth}px`,
  ].join(" ");

  return (
    <div
      ref={appRef}
      className="app"
      style={{ gridTemplateColumns }}
      data-tauri-drag-region
    >
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
        activeSettingsTab={view === "settings" ? settingsTab : null}
        onSelect={(id) => {
          setActiveId(id);
          setView("thread");
        }}
        onNewChat={() => {
          newChat();
          setView("thread");
        }}
        onOpenSettings={() => openSettings("general")}
        onOpenModels={() => openSettings("models")}
        onOpenEvaluations={() => openSettings("evaluations")}
      />

      {!sidebarCollapsed && (
        <div
          className="island-resize-handle island-resize-handle-left"
          style={{ left: 12 + sidebarWidth }}
          role="separator"
          tabIndex={0}
          aria-label="Resize workspace sidebar"
          aria-orientation="vertical"
          aria-valuemin={MIN_ISLAND_WIDTH}
          aria-valuemax={MAX_ISLAND_WIDTH}
          aria-valuenow={Math.round(sidebarWidth)}
          onPointerDown={(event) => beginIslandResize("left", event)}
          onKeyDown={(event) => resizeIslandWithKeyboard("left", event)}
        />
      )}

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
          <>
            <ChatView
              status={status}
              session={active}
              gitStatus={gitStatus}
              onUpdateMessages={setMessages}
              onEnsureSession={newChat}
              onStreamingChange={setStreamingId}
              onSetPermission={setPermission}
              onGitRefresh={refreshGit}
              gitBusy={activeProjectBusy}
            />
            {active && <RuntimeDock session={active} onGitRefresh={() => void refreshGit()} onChatsRefresh={() => void refreshSessions()} />}
          </>
        )}
      </main>

      <RightPanel
        status={status}
        session={effectiveView === "thread" ? active : null}
        mode={rightPanelMode}
        onModeChange={setRightPanelMode}
        gitRefreshVersion={gitRefreshVersion}
        gitBusy={activeProjectBusy}
        onGitChanged={refreshGitStatus}
      />
      {rightPanelMode !== "collapsed" && (
        <div
          className="island-resize-handle island-resize-handle-right"
          style={{ right: 12 + rightPanelWidth }}
          role="separator"
          tabIndex={0}
          aria-label="Resize secondary sidebar"
          aria-orientation="vertical"
          aria-valuemin={MIN_ISLAND_WIDTH}
          aria-valuemax={MAX_ISLAND_WIDTH}
          aria-valuenow={Math.round(rightPanelWidth)}
          onPointerDown={(event) => beginIslandResize("right", event)}
          onKeyDown={(event) => resizeIslandWithKeyboard("right", event)}
        />
      )}
    </div>
  );
}
