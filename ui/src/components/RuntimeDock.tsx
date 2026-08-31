import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Columns2,
  ExternalLink,
  PanelBottomOpen,
  Pause,
  Play,
  Plus,
  Activity,
  RefreshCw,
  RotateCcw,
  ShieldCheck,
  SquareTerminal,
  Trash2,
  X,
} from "lucide-react";
import { Terminal as XTerminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import {
  api,
  subscribeSse,
  type CheckpointSummary,
  type GenerationState,
  type PolicyDocument,
  type PreviewProbe,
  type RuntimeDockTab,
  type RuntimeEvent,
  type TerminalInfo,
  type TerminalOutput,
} from "../api";
import type { ChatSession } from "../sessions";
import { normalizePreviewUrl } from "../previewUrl";
import { useDialogs } from "./Dialogs";
import { Select } from "./Select";
import { useSettings } from "../settings";

const MIN_DOCK_HEIGHT = 160;
const HIDE_DOCK_HEIGHT = 92;

export function RuntimeDock({
  session,
  onGitRefresh,
  onChatsRefresh,
}: {
  session: ChatSession;
  onGitRefresh: () => void;
  onChatsRefresh: () => void;
}) {
  const { settings, save } = useSettings();
  const { toast } = useDialogs();
  const [height, setHeight] = useState(() => Math.max(MIN_DOCK_HEIGHT, settings.runtime_dock_height));
  const [dragging, setDragging] = useState(false);
  // The runtime area is intentionally absent on launch. Its opener is
  // separate from the Terminal tab because it reveals all runtime tools.
  const [open, setOpen] = useState(false);
  const tab: RuntimeDockTab = settings.runtime_dock_tab === "timeline" || settings.runtime_dock_tab === "safety"
    ? settings.runtime_dock_tab
    : "terminal";

  const updateSettings = useCallback(
    (patch: Partial<typeof settings>) => {
      void save({ ...settings, ...patch }).catch((error) =>
        toast(error instanceof Error ? error.message : String(error), "error"),
      );
    },
    [save, settings, toast],
  );

  const selectTab = (next: RuntimeDockTab) => updateSettings({ runtime_dock_tab: next });

  const beginResize = (event: React.PointerEvent) => {
    event.preventDefault();
    const originY = event.clientY;
    const originHeight = height;
    const max = Math.max(MIN_DOCK_HEIGHT, window.innerHeight * 0.72);
    setDragging(true);
    const move = (next: PointerEvent) => {
      setHeight(Math.min(max, Math.max(48, originHeight + originY - next.clientY)));
    };
    const finish = (next: PointerEvent) => {
      const nextHeight = Math.min(max, Math.max(48, originHeight + originY - next.clientY));
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", finish);
      setDragging(false);
      if (nextHeight < HIDE_DOCK_HEIGHT) {
        setHeight(Math.max(MIN_DOCK_HEIGHT, settings.runtime_dock_height));
        setOpen(false);
      } else {
        setHeight(nextHeight);
        updateSettings({ runtime_dock_height: Math.round(nextHeight) });
      }
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", finish);
  };

  if (!open) {
    return (
      <button
        type="button"
        className="runtime-dock-launcher"
        title="Open runtime tools"
        aria-label="Open runtime tools"
        aria-controls="workspace-runtime-dock"
        aria-expanded="false"
        onClick={() => setOpen(true)}
      >
        <PanelBottomOpen size={15} />
        <span>Runtime</span>
      </button>
    );
  }

  return (
    <section
      id="workspace-runtime-dock"
      className={`runtime-dock ${dragging ? "runtime-dock-dragging" : ""}`}
      style={{ height }}
    >
      <div className="runtime-dock-resizer" onPointerDown={beginResize} />
      <header className="runtime-dock-header">
        <nav className="runtime-dock-tabs" aria-label="Workspace runtime">
          <DockTab icon={<SquareTerminal size={14} />} active={tab === "terminal"} onClick={() => selectTab("terminal")}>Terminal</DockTab>
          <DockTab icon={<Activity size={14} />} active={tab === "timeline"} onClick={() => selectTab("timeline")}>Timeline</DockTab>
          <DockTab icon={<ShieldCheck size={14} />} active={tab === "safety"} onClick={() => selectTab("safety")}>Safety</DockTab>
        </nav>
        <button
          className="runtime-icon-btn"
          type="button"
          title="Hide runtime tools"
          aria-label="Hide runtime tools"
          onClick={() => setOpen(false)}
        >
          <X size={14} />
        </button>
      </header>
      <div className="runtime-dock-content">
        {tab === "terminal" && <TerminalIsland projectId={session.projectId} />}
        {tab === "timeline" && <TimelineIsland sessionId={session.id} />}
        {tab === "safety" && (
          <SafetyIsland session={session} onGitRefresh={onGitRefresh} onChatsRefresh={onChatsRefresh} />
        )}
      </div>
    </section>
  );
}

function DockTab({ active, icon, children, onClick }: { active: boolean; icon: React.ReactNode; children: React.ReactNode; onClick: () => void }) {
  return <button type="button" className={`runtime-dock-tab ${active ? "active" : ""}`} onClick={onClick}>{icon}{children}</button>;
}

function TerminalIsland({ projectId }: { projectId: string }) {
  const { toast } = useDialogs();
  const [terminals, setTerminals] = useState<TerminalInfo[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [splitId, setSplitId] = useState<string | null>(null);
  const autoStartPending = useRef(false);
  const refresh = useCallback(() => {
    api.listTerminals(projectId).then(({ terminals: next }) => {
      if (next.length === 0 && !autoStartPending.current) {
        autoStartPending.current = true;
        api.createTerminal(projectId).then((terminal) => {
          autoStartPending.current = false;
          setTerminals([terminal]);
          setActiveId(terminal.id);
        }).catch((error) => {
          autoStartPending.current = false;
          toast(error instanceof Error ? error.message : String(error), "error");
        });
        return;
      }
      setTerminals(next);
      setActiveId((current) => next.some((item) => item.id === current) ? current : next[0]?.id ?? null);
      setSplitId((current) => next.some((item) => item.id === current) ? current : null);
    }).catch((error) => toast(error instanceof Error ? error.message : String(error), "error"));
  }, [projectId, toast]);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, 4000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const create = async () => {
    try {
      const terminal = await api.createTerminal(projectId);
      setTerminals((items) => [
        ...items.filter((item) => item.id !== terminal.id),
        terminal,
      ]);
      setActiveId(terminal.id);
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };
  const close = async (id: string) => {
    try {
      await api.closeTerminal(id);
      const next = terminals.filter((item) => item.id !== id);
      setTerminals(next);
      if (activeId === id) setActiveId(next[0]?.id ?? null);
      if (splitId === id) setSplitId(null);
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };
  const active = terminals.find((item) => item.id === activeId);
  const split = terminals.find((item) => item.id === splitId);
  const toggleSplit = () => {
    if (splitId) setSplitId(null);
    else setSplitId(terminals.find((item) => item.id !== activeId)?.id ?? null);
  };

  return (
    <div className="terminal-island">
      <div className="terminal-toolbar">
        <div className="terminal-tabs">
          {terminals.map((terminal) => (
            <button key={terminal.id} type="button" className={`terminal-tab ${terminal.id === activeId ? "active" : ""}`} onClick={() => setActiveId(terminal.id)}>
              <span className={`runtime-dot ${terminal.running ? "live" : ""}`} />
              {terminal.title}
              <X size={12} onClick={(event) => { event.stopPropagation(); void close(terminal.id); }} />
            </button>
          ))}
        </div>
        <button type="button" className="runtime-icon-btn" title="New terminal" onClick={() => void create()}><Plus size={14} /></button>
        <button type="button" className={`runtime-icon-btn ${splitId ? "active" : ""}`} title="Split terminal" disabled={terminals.length < 2} onClick={toggleSplit}><Columns2 size={14} /></button>
      </div>
      {active ? (
        <div className={`terminal-panes ${split ? "split" : ""}`}>
          <TerminalPane key={active.id} terminal={active} />
          {split && <TerminalPane key={split.id} terminal={split} />}
        </div>
      ) : <div className="runtime-empty terminal-starting"><SquareTerminal size={18} /> Starting terminal…</div>}
    </div>
  );
}

function TerminalPane({ terminal }: { terminal: TerminalInfo }) {
  const host = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!host.current) return;
    const xterm = new XTerminal({
      cursorBlink: true,
      fontFamily: '"SF Mono", ui-monospace, monospace',
      fontSize: 12,
      lineHeight: 1.2,
      scrollback: 5000,
      theme: { background: "#090909", foreground: "#d1d5db", cursor: "#60a5fa", selectionBackground: "#1d4ed880" },
    });
    const fit = new FitAddon();
    xterm.loadAddon(fit);
    xterm.open(host.current);
    const decoder = new TextDecoder();
    const unsubscribe = subscribeSse(api.terminalStreamUrl(terminal.id), (raw) => {
      const chunk = JSON.parse(raw) as TerminalOutput;
      const bytes = Uint8Array.from(atob(chunk.data), (character) => character.charCodeAt(0));
      xterm.write(decoder.decode(bytes, { stream: true }));
    }, () => xterm.write("\r\n\x1b[33mTerminal stream disconnected.\x1b[0m\r\n"));
    const input = xterm.onData((data) => void api.terminalInput(terminal.id, data));
    let resizeTimer = 0;
    const resize = new ResizeObserver(() => {
      fit.fit();
      window.clearTimeout(resizeTimer);
      resizeTimer = window.setTimeout(() => void api.resizeTerminal(terminal.id, xterm.cols, xterm.rows), 120);
    });
    resize.observe(host.current);
    fit.fit();
    xterm.focus();
    return () => {
      window.clearTimeout(resizeTimer);
      resize.disconnect();
      input.dispose();
      unsubscribe();
      xterm.dispose();
    };
  }, [terminal.id]);
  return <div className="terminal-pane" ref={host} />;
}

export function PreviewIsland({ projectId }: { projectId: string }) {
  const { toast } = useDialogs();
  const [terminals, setTerminals] = useState<TerminalInfo[]>([]);
  const urls = useMemo(() => Array.from(new Set(
    terminals
      .flatMap((terminal) => terminal.detected_urls)
      .map(normalizePreviewUrl)
      .filter((item): item is string => item !== null),
  )), [terminals]);
  const [url, setUrl] = useState("");
  const [draft, setDraft] = useState("");
  const [probe, setProbe] = useState<PreviewProbe | null>(null);
  const [reload, setReload] = useState(0);
  const [viewport, setViewport] = useState<"responsive" | "tablet" | "mobile">("responsive");
  useEffect(() => {
    const refresh = () => api.listTerminals(projectId).then(({ terminals: next }) => {
      setTerminals(next);
      const detected = next
        .flatMap((item) => item.detected_urls)
        .map(normalizePreviewUrl)
        .find((item): item is string => item !== null);
      if (detected) setUrl((current) => current || detected);
    }).catch(() => {});
    refresh();
    const timer = window.setInterval(refresh, 3000);
    return () => window.clearInterval(timer);
  }, [projectId]);
  useEffect(() => {
    if (!url) return;
    const check = () => api.previewProbe(url).then(setProbe).catch(() => setProbe({ reachable: false, status: null, latency_ms: 0, content_type: null }));
    check();
    const timer = window.setInterval(check, 5000);
    return () => window.clearInterval(timer);
  }, [url, reload]);
  const navigate = () => {
    const value = normalizePreviewUrl(draft);
    if (!value) {
      toast("Preview URLs must use localhost or a loopback address", "error");
      return;
    }
    setUrl(value);
  };
  return <div className="preview-island">
    <div className="preview-toolbar">
      <Select
        value={url}
        ariaLabel="Detected preview URL"
        triggerClassName="runtime-select preview-url-select"
        onChange={(value) => { setUrl(value); if (!value) setProbe(null); }}
        options={[
          { value: "", label: "Detected URLs" },
          ...urls.map((item) => ({ value: item, label: item })),
        ]}
      />
      <input value={draft} placeholder="localhost:3000" onChange={(event) => setDraft(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") navigate(); }} />
      <button type="button" className="btn" onClick={navigate}>Open</button>
      <button type="button" className="runtime-icon-btn" title="Reload" onClick={() => setReload((value) => value + 1)}><RefreshCw size={14} /></button>
      <div className="preview-toolbar-meta"><Select value={viewport} onChange={setViewport} ariaLabel="Preview viewport" triggerClassName="runtime-select" options={[{ value: "responsive", label: "Responsive" }, { value: "tablet", label: "Tablet · 768 px" }, { value: "mobile", label: "Mobile · 390 px" }]} />
        <span className={`preview-health ${probe?.reachable ? "good" : "bad"}`}>{probe ? `${probe.status ?? "offline"} · ${probe.latency_ms} ms` : "No preview"}</span>
        {url && <a className="runtime-icon-btn" href={url} target="_blank" rel="noreferrer" title="Open in browser"><ExternalLink size={14} /></a>}
      </div>
    </div>
    <div className="preview-stage"><div className={`preview-frame ${viewport}`}><iframe key={`${url}-${reload}`} title="Workspace preview" src={url || "about:blank"} /></div></div>
  </div>;
}

function TimelineIsland({ sessionId }: { sessionId: string }) {
  const { toast } = useDialogs();
  const [events, setEvents] = useState<RuntimeEvent[]>([]);
  const [generation, setGeneration] = useState<GenerationState | null>(null);
  const [followup, setFollowup] = useState("");
  useEffect(() => {
    let last = 0;
    api.runtimeEvents(sessionId).then(({ events: history }) => {
      setEvents(history);
      last = history.at(-1)?.sequence ?? 0;
    }).catch(() => {});
    const unsubscribe = subscribeSse(api.runtimeEventsUrl(sessionId, last), (raw) => {
      const event = JSON.parse(raw) as RuntimeEvent;
      setEvents((items) => [...items.filter((item) => item.sequence !== event.sequence), event].slice(-500));
    }, () => {});
    const poll = () => api.generation(sessionId).then(setGeneration).catch(() => {});
    poll();
    const timer = window.setInterval(poll, 1000);
    return () => { unsubscribe(); window.clearInterval(timer); };
  }, [sessionId]);
  const control = async (action: string, text?: string) => {
    try { setGeneration(await api.controlGeneration(sessionId, action, text)); }
    catch (error) { toast(error instanceof Error ? error.message : String(error), "error"); }
  };
  return <div className="timeline-island">
    <div className="timeline-controls">
      <span className={`runtime-status ${generation?.running ? "live" : ""}`}>{generation?.running ? generation.paused ? "Paused" : "Running" : "Idle"}</span>
      <button type="button" className="btn" disabled={!generation?.running} onClick={() => void control(generation?.paused ? "resume" : "pause")}>{generation?.paused ? <Play size={13} /> : <Pause size={13} />}{generation?.paused ? "Resume" : "Pause"}</button>
      <button type="button" className="btn" disabled={!generation?.running || generation.stop_after_step} onClick={() => void control("stop_after_step")}>Stop after step</button>
      <input value={followup} disabled={!generation?.running} placeholder="Queue guidance for the next step" onChange={(event) => setFollowup(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter" && followup.trim()) { void control("queue", followup.trim()); setFollowup(""); } }} />
      <button type="button" className="btn btn-accent" disabled={!generation?.running || !followup.trim()} onClick={() => { void control("queue", followup.trim()); setFollowup(""); }}>Queue</button>
      {!!generation?.queued_followups.length && <span className="runtime-muted">{generation.queued_followups.length} queued</span>}
    </div>
    <div className="timeline-list">{events.length ? events.slice().reverse().map((event) => <article key={event.sequence} className="timeline-event"><span className={`timeline-topic topic-${event.topic}`}>{event.topic}</span><strong>{labelEvent(event.action)}</strong><time>{new Date(event.emitted_at).toLocaleTimeString()}</time><code>{eventSummary(event.data)}</code></article>) : <div className="runtime-empty">Runtime activity will appear here.</div>}</div>
  </div>;
}

function SafetyIsland({ session, onGitRefresh, onChatsRefresh }: { session: ChatSession; onGitRefresh: () => void; onChatsRefresh: () => void }) {
  const { confirm, toast } = useDialogs();
  const [checkpoints, setCheckpoints] = useState<CheckpointSummary[]>([]);
  const [document, setDocument] = useState<PolicyDocument | null>(null);
  const [restoreMode, setRestoreMode] = useState<"files" | "conversation" | "both">("both");
  const refresh = useCallback(() => {
    api.listCheckpoints(session.id).then(({ checkpoints: next }) => setCheckpoints(next)).catch(() => {});
    api.policies().then(setDocument).catch(() => {});
  }, [session.id]);
  useEffect(refresh, [refresh]);
  const restore = async (checkpoint: CheckpointSummary) => {
    const approved = await confirm({ title: "Restore this checkpoint?", message: `This restores ${checkpoint.paths.join(", ")}. A safety checkpoint will preserve the current state.`, confirmLabel: "Restore", danger: true });
    if (!approved) return;
    try { await api.restoreCheckpoint(checkpoint.id, session.id, session.projectId, restoreMode); toast("Checkpoint restored", "success"); refresh(); onGitRefresh(); onChatsRefresh(); }
    catch (error) { toast(error instanceof Error ? error.message : String(error), "error"); }
  };
  const saveDocument = async (next: PolicyDocument) => {
    setDocument(next);
    try { setDocument(await api.savePolicies(next)); toast("Safety policy saved", "success"); }
    catch (error) { toast(error instanceof Error ? error.message : String(error), "error"); refresh(); }
  };
  return <div className="safety-island">
    <section className="safety-column"><div className="runtime-section-title"><span>Rewind checkpoints</span><div className="runtime-title-actions"><Select value={restoreMode} onChange={setRestoreMode} ariaLabel="Checkpoint restore scope" triggerClassName="runtime-select" options={[{ value: "both", label: "Code + chat" }, { value: "files", label: "Code only" }, { value: "conversation", label: "Chat only" }]} /><button className="runtime-icon-btn" type="button" onClick={refresh}><RefreshCw size={13} /></button></div></div><div className="checkpoint-list">{checkpoints.map((checkpoint) => <article className="checkpoint-card" key={checkpoint.id}><div><strong>{labelEvent(checkpoint.operation)}</strong><time>{new Date(checkpoint.created_at).toLocaleString()}</time></div><code>{checkpoint.paths.join(", ")}</code><button className="btn" type="button" onClick={() => void restore(checkpoint)}><RotateCcw size={13} /> Restore</button></article>)}{!checkpoints.length && <div className="runtime-empty">File-changing tools create checkpoints automatically.</div>}</div></section>
    <section className="safety-column">{document && <PolicyEditor document={document} projectId={session.projectId} onChange={setDocument} onSave={() => void saveDocument(document)} />}</section>
  </div>;
}

function PolicyEditor({ document, projectId, onChange, onSave }: { document: PolicyDocument; projectId: string; onChange: (next: PolicyDocument) => void; onSave: () => void }) {
  const addRule = () => onChange({ ...document, rules: [...document.rules, { id: crypto.randomUUID(), name: "Workspace rule", enabled: true, project_id: projectId, effect: "ask", tool_pattern: "*", target_pattern: "*" }] });
  const updateRule = (id: string, patch: Record<string, unknown>) => onChange({ ...document, rules: document.rules.map((rule) => rule.id === id ? { ...rule, ...patch } : rule) });
  const removeRule = (id: string) => onChange({ ...document, rules: document.rules.filter((rule) => rule.id !== id) });
  const addHook = () => onChange({ ...document, hooks: [...document.hooks, { id: crypto.randomUUID(), name: "Workspace hook", enabled: true, project_id: projectId, phase: "post_tool", tool_pattern: "*", kind: "command", target: "", blocking: false, timeout_ms: 5000 }] });
  const updateHook = (id: string, patch: Record<string, unknown>) => onChange({ ...document, hooks: document.hooks.map((hook) => hook.id === id ? { ...hook, ...patch } : hook) });
  const removeHook = (id: string) => onChange({ ...document, hooks: document.hooks.filter((hook) => hook.id !== id) });
  return <><div className="runtime-section-title"><span>Policies</span><div className="runtime-title-actions"><button className="btn" type="button" onClick={addRule}><Plus size={13} /> Rule</button><button className="btn btn-accent" type="button" onClick={onSave}>Save policy</button></div></div><div className="policy-list">{document.rules.map((rule) => <article className="policy-card" key={rule.id}><input value={rule.name} onChange={(event) => updateRule(rule.id, { name: event.target.value })} /><Select value={rule.effect} onChange={(effect) => updateRule(rule.id, { effect })} ariaLabel="Policy effect" triggerClassName="runtime-select" options={[{ value: "allow", label: "Allow" }, { value: "ask", label: "Ask" }, { value: "deny", label: "Deny" }]} /><input className="mono" value={rule.tool_pattern} title="Tool pattern" onChange={(event) => updateRule(rule.id, { tool_pattern: event.target.value })} /><input className="mono" value={rule.target_pattern} title="Target pattern" onChange={(event) => updateRule(rule.id, { target_pattern: event.target.value })} /><button className="runtime-icon-btn" type="button" onClick={() => removeRule(rule.id)}><Trash2 size={13} /></button></article>)}</div><div className="runtime-section-title"><span>Lifecycle hooks</span><button className="btn" type="button" onClick={addHook}><Plus size={13} /> Hook</button></div><div className="policy-list">{document.hooks.map((hook) => <article className="policy-card hook" key={hook.id}><input value={hook.name} onChange={(event) => updateHook(hook.id, { name: event.target.value })} /><Select value={hook.phase} onChange={(phase) => updateHook(hook.id, { phase })} ariaLabel="Hook phase" triggerClassName="runtime-select" options={[{ value: "pre_tool", label: "Before tool" }, { value: "post_tool", label: "After tool" }, { value: "run_completed", label: "Run completed" }, { value: "checkpoint_restored", label: "Checkpoint restored" }, { value: "terminal_started", label: "Terminal started" }]} /><Select value={hook.kind} onChange={(kind) => updateHook(hook.id, { kind })} ariaLabel="Hook kind" triggerClassName="runtime-select" options={[{ value: "command", label: "Command" }, { value: "http", label: "Webhook" }]} /><input className="mono" value={hook.target} placeholder={hook.kind === "command" ? "npm test" : "https://..."} onChange={(event) => updateHook(hook.id, { target: event.target.value })} /><label><input type="checkbox" checked={hook.blocking} onChange={(event) => updateHook(hook.id, { blocking: event.target.checked })} /> block on failure</label><button className="runtime-icon-btn" type="button" onClick={() => removeHook(hook.id)}><Trash2 size={13} /></button></article>)}</div></>;
}

function labelEvent(value: string) { return value.replaceAll("_", " ").replace(/\b\w/g, (letter) => letter.toUpperCase()); }
function eventSummary(data: Record<string, unknown>) { return Object.entries(data).slice(0, 3).map(([key, value]) => `${key}: ${typeof value === "string" ? value : JSON.stringify(value)}`).join(" · "); }
