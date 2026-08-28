import { useEffect, useRef, useState } from "react";
import {
  ArrowUp,
  Brain,
  Check,
  ChevronDown,
  ChevronRight,
  Eye,
  GitBranch,
  Lock,
  MoreHorizontal,
  Pencil,
  Sparkles,
  Unlock,
  X,
} from "lucide-react";
import { api, streamChat, type GitStatus, type StatusResponse, type ToolStep } from "../api";
import { compactMessages, shouldAutoCompact } from "../compact";
import { formatContextLength, formatDuration, groupBySection, modelName } from "../format";
import { changedFilesOf, codeBlockCountOf, stripCodeBlocks } from "../diff";
import type { ChatSession, StoredMessage } from "../sessions";
import { useSettings } from "../settings";
import { Select } from "../components/Select";
import { Markdown } from "../components/Markdown";

function openRaw(text: string) {
  const url = URL.createObjectURL(new Blob([text], { type: "text/markdown" }));
  window.open(url, "_blank");
}

function openDiff(files: { filename: string; body: string }[]) {
  const text = files.map((f) => `--- ${f.filename}\n${f.body}`).join("\n\n");
  openRaw(text);
}

export function ChatView({
  status,
  session,
  gitStatus,
  onUpdateMessages,
  onEnsureSession,
  onStreamingChange,
  onTogglePermission,
}: {
  status: StatusResponse | null;
  session: ChatSession | null;
  gitStatus: GitStatus | null;
  onUpdateMessages: (id: string, messages: StoredMessage[]) => void;
  onEnsureSession: () => Promise<ChatSession>;
  onStreamingChange: (id: string | null) => void;
  onTogglePermission: (id: string) => void;
}) {
  const { settings } = useSettings();
  const models = status?.models ?? [];
  const [model, setModel] = useState("");
  // Off by default (matches the daemon's default): a Qwen3-style hidden
  // <think> pass before the real answer, only useful for reasoning models.
  const [thinking, setThinking] = useState(false);
  const [input, setInput] = useState("");
  const [previewing, setPreviewing] = useState(false);
  const [liveText, setLiveText] = useState("");
  const [sending, setSending] = useState(false);
  const [compacting, setCompacting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const [streamingSessionId, setStreamingSessionId] = useState<string | null>(null);
  const [openCode, setOpenCode] = useState<Record<number, boolean>>({});
  const [openFiles, setOpenFiles] = useState<Record<number, boolean>>({});
  const [openSteps, setOpenSteps] = useState<Record<number, boolean>>({});
  const [overflowOpen, setOverflowOpen] = useState(false);
  const liveStepsRef = useRef<ToolStep[]>([]);

  const effectiveModel = models.find((m) => m.id === model)?.id || models[0]?.id || "";
  const effectiveModelObj = models.find((m) => m.id === effectiveModel);

  // Manual compaction: typing "/compact" replaces the older half of the
  // chat with one model-written summary instead of sending it as a
  // message. Same routine `send()` below runs automatically when
  // settings.auto_compact is on and the history outgrows the model's
  // context window.
  const compact = async () => {
    if (!effectiveModel || !session || compacting) return;
    setInput("");
    setPreviewing(false);
    setError(null);
    setCompacting(true);
    try {
      onUpdateMessages(session.id, await compactMessages(effectiveModel, session.messages));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setCompacting(false);
    }
  };

  const send = async () => {
    const text = input.trim();
    if (!text || !effectiveModel || sending) return;
    if (text.toLowerCase() === "/compact") {
      await compact();
      return;
    }
    const target = session ?? (await onEnsureSession());
    setError(null);
    setInput("");
    setPreviewing(false);

    let baseMessages = target.messages;
    if (settings.auto_compact && shouldAutoCompact(baseMessages, effectiveModelObj?.context_length)) {
      try {
        baseMessages = await compactMessages(effectiveModel, baseMessages);
        onUpdateMessages(target.id, baseMessages);
      } catch {
        // Best-effort: send the uncompacted history rather than block the turn over it.
      }
    }
    const startMessages: StoredMessage[] = [...baseMessages, { role: "user", content: text }];
    onUpdateMessages(target.id, startMessages);

    setSending(true);
    setStreamingSessionId(target.id);
    onStreamingChange(target.id);
    const controller = new AbortController();
    abortRef.current = controller;
    const start = performance.now();
    let assistantText = "";
    liveStepsRef.current = [];
    try {
      await streamChat(
        effectiveModel,
        startMessages.map(({ role, content }) => ({ role, content })),
        target.id,
        (token) => {
          assistantText += token;
          setLiveText(assistantText);
        },
        controller.signal,
        target.projectId,
        target.permission,
        (steps) => {
          liveStepsRef.current = steps;
        },
        thinking,
      );
    } catch (err) {
      if (!controller.signal.aborted) {
        setError(err instanceof Error ? err.message : String(err));
      }
    } finally {
      const durationMs = performance.now() - start;
      onUpdateMessages(target.id, [
        ...startMessages,
        { role: "assistant", content: assistantText, durationMs, steps: liveStepsRef.current },
      ]);
      setLiveText("");
      setSending(false);
      onStreamingChange(null);
      setStreamingSessionId(null);
    }
  };

  const stop = () => {
    abortRef.current?.abort(); // detach this tab's own view immediately
    // Generation itself is server-side now (decoupled from this
    // connection) -- actually stop it too, not just look away.
    if (streamingSessionId) api.stopGeneration(streamingSessionId).catch(() => {});
  };

  // Catches this session up with the daemon's server-side generation
  // record: covers switching back to a session that kept generating (or
  // finished) while unfocused, and surviving a full page reload -- the
  // daemon's buffer is the source of truth, not this component's memory.
  // Skipped whenever this tab is itself the one driving the live stream
  // for this exact session (its own `send()` above already handles it).
  useEffect(() => {
    if (!session) return;
    const sessionId = session.id;
    const last = session.messages[session.messages.length - 1];
    if (last?.role !== "user") return; // nothing pending a reply
    if (sending && streamingSessionId === sessionId) return;

    let cancelled = false;
    let timer: number | undefined;
    const poll = async () => {
      const snap = await api.generation(sessionId).catch(() => null);
      if (cancelled || !snap || !snap.model) return;
      if (snap.running) {
        setLiveText(snap.text);
        setSending(true);
        setStreamingSessionId(sessionId);
        onStreamingChange(sessionId);
        timer = window.setTimeout(poll, 500);
        return;
      }
      setLiveText("");
      setSending(false);
      setStreamingSessionId((id) => (id === sessionId ? null : id));
      onStreamingChange(null);
      if (snap.text) {
        onUpdateMessages(sessionId, [
          ...session.messages,
          { role: "assistant", content: snap.text, steps: snap.steps },
        ]);
      } else if (snap.error) {
        setError(snap.error);
      }
    };
    poll();
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session?.id, session?.messages.length]);

  const isStreamingThisSession = sending && streamingSessionId === session?.id;
  const messages = session?.messages ?? [];

  // Everything except the model picker: collapses into the "..."
  // overflow menu on a narrow composer instead of wrapping/overflowing
  // (see .composer-chips-wide / .composer-overflow in app.css).
  const renderConfigChips = () => (
    <>
      <button
        type="button"
        className={thinking ? "chip-select chip-toggle chip-toggle-on" : "chip-select chip-toggle"}
        title="Let the model think before answering (reasoning models only)"
        onClick={() => setThinking((t) => !t)}
      >
        {thinking ? (
          <>
            <Brain size={12} /> Thinking
          </>
        ) : (
          "Thinking: off"
        )}
      </button>
      {effectiveModelObj && (
        <span className="chip-select chip-static" title="Trained context window, from the model's GGUF metadata">
          {formatContextLength(effectiveModelObj.context_length)}
        </span>
      )}
      <button
        type="button"
        className={
          session?.permission === "full-access" ? "chip-select chip-toggle chip-toggle-on" : "chip-select chip-toggle"
        }
        title="Whether the model can write files or run commands in this workspace, or only read them"
        onClick={() => session && onTogglePermission(session.id)}
      >
        {session?.permission === "full-access" ? (
          <>
            <Unlock size={12} /> Full access
          </>
        ) : (
          <>
            <Lock size={12} /> Read only
          </>
        )}
        <ChevronDown size={12} />
      </button>
    </>
  );

  return (
    <div className="thread-col">
      <div className="chat-log">
        {messages.length === 0 && !isStreamingThisSession && (
          <div className="chat-empty">Send a message to start a new chat.</div>
        )}
        {messages.map((m, i) =>
          m.role === "user" ? (
            <div key={i} className="turn-user">
              <div className="chat-bubble chat-bubble-user">
                <Markdown>{m.content}</Markdown>
              </div>
            </div>
          ) : (
            <div key={i} className="turn-assistant">
              {stripCodeBlocks(m.content) && (
                <Markdown className="chat-text">{stripCodeBlocks(m.content)}</Markdown>
              )}
              {typeof m.durationMs === "number" && (
                <>
                  {m.steps && m.steps.length > 0 ? (
                    <button
                      type="button"
                      className="worked-line worked-line-toggle"
                      onClick={() => setOpenSteps((o) => ({ ...o, [i]: !o[i] }))}
                    >
                      Worked for {formatDuration(m.durationMs)}
                      <span className={openSteps[i] ? "worked-chevron worked-chevron-open" : "worked-chevron"}>
                        <ChevronRight size={12} />
                      </span>
                    </button>
                  ) : (
                    <div className="worked-line">
                      Worked for {formatDuration(m.durationMs)}{" "}
                      <span className="worked-chevron">
                        <ChevronRight size={12} />
                      </span>
                    </div>
                  )}
                  {openSteps[i] && m.steps && m.steps.length > 0 && (
                    <ul className="tool-step-list">
                      {m.steps.map((s, si) => (
                        <li key={si} className={s.ok ? "tool-step" : "tool-step tool-step-error"}>
                          <span className="tool-step-icon">{s.ok ? <Check size={12} /> : <X size={12} />}</span>
                          <span className="tool-step-name mono">{s.name}</span>
                          <span className="tool-step-summary">{s.result_summary}</span>
                        </li>
                      ))}
                    </ul>
                  )}
                </>
              )}
              {changedFilesOf(m.content).length > 0 && (
                <div className="changed-block">
                  <button
                    type="button"
                    className="changed-block-toggle"
                    onClick={() => setOpenFiles((o) => ({ ...o, [i]: !o[i] }))}
                  >
                    <span className={openFiles[i] ? "file-chevron file-chevron-open" : "file-chevron"}>
                      <ChevronRight size={14} />
                    </span>
                    {changedFilesOf(m.content).length} changed file
                    {changedFilesOf(m.content).length > 1 ? "s" : ""}
                    <span className="changed-block-hint">{openFiles[i] ? "Hide files" : "Show files"}</span>
                  </button>
                  <button
                    type="button"
                    className="btn btn-ghost changed-block-open"
                    onClick={() => openDiff(changedFilesOf(m.content))}
                  >
                    Open diff
                  </button>
                </div>
              )}
              {openFiles[i] && (
                <ul className="file-list">
                  {changedFilesOf(m.content).map((f) => (
                    <li key={f.filename} className="file-row">
                      <div className="file-row-head">
                        <span className="file-name mono truncate">{f.filename}</span>
                        <span className="file-stats">
                          <span className="stat-add">+{f.added}</span>
                          <span className="stat-del">-0</span>
                        </span>
                      </div>
                    </li>
                  ))}
                </ul>
              )}
              {codeBlockCountOf(m.content) > 0 && (
                <div className="changed-block">
                  <button
                    type="button"
                    className="changed-block-toggle"
                    onClick={() => setOpenCode((o) => ({ ...o, [i]: !o[i] }))}
                  >
                    <span className={openCode[i] ? "file-chevron file-chevron-open" : "file-chevron"}>
                      <ChevronRight size={14} />
                    </span>
                    {codeBlockCountOf(m.content)} code block{codeBlockCountOf(m.content) > 1 ? "s" : ""}
                    <span className="changed-block-hint">{openCode[i] ? "Hide code" : "Show code"}</span>
                  </button>
                  <button
                    type="button"
                    className="btn btn-ghost changed-block-open"
                    onClick={() => openRaw(m.content)}
                  >
                    Open raw
                  </button>
                </div>
              )}
              {openCode[i] && <pre className="changed-block-code mono">{m.content}</pre>}
            </div>
          ),
        )}
        {isStreamingThisSession && (
          <div className="turn-assistant">
            {liveText ? <Markdown className="chat-text">{liveText}</Markdown> : <div className="chat-text">…</div>}
          </div>
        )}
      </div>

      {error && <div className="banner banner-bad">{error}</div>}

      <div className="composer">
        <form
          className="composer-box"
          onSubmit={(e) => {
            e.preventDefault();
            send();
          }}
        >
          {isStreamingThisSession && (
            <div className="bg-work-strip">
              <span className="bg-work-status">
                <span className="dot dot-good" /> Background work
              </span>
              <button type="button" className="btn btn-ghost" onClick={stop}>
                Stop
              </button>
            </div>
          )}
          {previewing ? (
            <Markdown className="composer-input composer-preview">
              {input || "Nothing to preview yet."}
            </Markdown>
          ) : (
            <textarea
              className="composer-input"
              placeholder={
                compacting
                  ? "Compacting chat…"
                  : effectiveModel
                    ? "Ask about your project, a file, or some connected tool… ('/compact' to summarize older messages)"
                    : "Pull a model first"
              }
              value={input}
              disabled={!effectiveModel || compacting}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key !== "Enter") return;
                // Default: Enter sends, Shift+Enter for a newline. If the
                // user's flipped "Enter sends" off in Settings, it's the
                // other way around: Enter is a newline, Ctrl/Cmd+Enter sends.
                const sends = settings.send_on_enter ? !e.shiftKey : e.metaKey || e.ctrlKey;
                if (sends) {
                  e.preventDefault();
                  send();
                }
              }}
            />
          )}
          <div className="composer-toolbar">
            <div className="composer-chips">
              <button
                type="button"
                className="chip-select composer-preview-toggle"
                title={previewing ? "Back to editing" : "Preview markdown"}
                onClick={() => setPreviewing((p) => !p)}
              >
                {previewing ? <Pencil size={12} /> : <Eye size={12} />}
                {previewing ? "Write" : "Preview"}
              </button>
              <Select
                triggerClassName="chip-select chip-select-model"
                icon={<Sparkles size={12} className="chip-select-flare" />}
                value={effectiveModel}
                onChange={setModel}
                options={models.length === 0 ? [{ value: "", label: "no models" }] : undefined}
                groups={
                  models.length === 0
                    ? undefined
                    : groupBySection(models, (m) => m.id).map((g) => ({
                        section: g.section,
                        options: g.items.map((m) => ({
                          value: m.id,
                          label: modelName(m.id),
                        })),
                      }))
                }
              />
              <div className="composer-chips-wide">{renderConfigChips()}</div>
              <div className="composer-overflow">
                <button
                  type="button"
                  className="chip-select composer-overflow-btn"
                  title="More options"
                  onClick={() => setOverflowOpen((o) => !o)}
                >
                  <MoreHorizontal size={14} />
                </button>
                {overflowOpen && (
                  <div className="composer-overflow-menu" onMouseLeave={() => setOverflowOpen(false)}>
                    {renderConfigChips()}
                  </div>
                )}
              </div>
            </div>
            <div className="composer-send-wrap">
              {/* Only this session's own stream, not a background one in another session. */}
              {isStreamingThisSession && <span className="composer-spinner" title="Generating" />}
              <button
                type="submit"
                className="composer-send"
                disabled={!effectiveModel || !input.trim() || sending || compacting}
                title="Send"
              >
                <ArrowUp size={14} />
              </button>
            </div>
          </div>
        </form>

        {gitStatus?.is_repo && (
          <div className="composer-git-row">
            <span className="composer-git-label">
              <span className="topbar-icon">
                <GitBranch size={12} />
              </span>{" "}
              Local checkout
            </span>
            <span className="composer-git-branch mono truncate">
              {gitStatus.branch ?? "detached HEAD"}
              {gitStatus.dirty ? " •" : ""}
            </span>
          </div>
        )}
      </div>
    </div>
  );
}
