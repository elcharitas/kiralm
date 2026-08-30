import { useEffect, useRef, useState, type DragEvent } from "react";
import {
  ArrowUp,
  Brain,
  Check,
  ChevronRight,
  Cloud,
  Copy,
  Cpu,
  Database,
  GitBranch,
  Lock,
  MoreHorizontal,
  Paperclip,
  Pencil,
  ShieldQuestion,
  Unlock,
  X,
} from "lucide-react";
import {
  api,
  streamChat,
  type AttachmentDescriptor,
  type ApprovalDecision,
  type ChatMessage,
  type GitBranchInfo,
  type GitStatus,
  type KnowledgeStatus,
  type PendingApproval,
  type PermissionMode,
  type StatusResponse,
  type ToolStep,
  type WorkspacePresetList,
} from "../api";
import { compactMessages, shouldAutoCompact } from "../compact";
import { formatContextLength, formatDuration, groupBySection, modelName } from "../format";
import { changedFilesOf, codeBlockCountOf, stripCodeBlocks } from "../diff";
import type { ChatSession, StoredMessage } from "../sessions";
import { useSettings } from "../settings";
import { Select } from "../components/Select";
import { Markdown } from "../components/Markdown";
import { useDialogs } from "../components/Dialogs";

function blobAsDataUrl(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error ?? new Error("File read failed"));
    reader.readAsDataURL(blob);
  });
}

async function messageForInference(message: StoredMessage): Promise<ChatMessage> {
  if (!message.attachments?.length) {
    return { role: message.role, content: message.content };
  }
  let text = message.content;
  const images: Array<{
    type: "image_url";
    image_url: { url: string };
  }> = [];
  for (const attachment of message.attachments) {
    const response = await fetch(api.attachmentUrl(attachment.id));
    if (!response.ok) throw new Error(`Could not read ${attachment.filename}`);
    if (attachment.kind === "image") {
      images.push({
        type: "image_url",
        image_url: { url: await blobAsDataUrl(await response.blob()) },
      });
    } else if (attachment.kind === "text") {
      const content = (await response.text()).slice(0, 100_000);
      text += `\n\n[Attached text: ${attachment.filename}]\n${content}`;
    } else {
      text += `\n\n[Attached document: ${attachment.filename}]`;
    }
  }
  return {
    role: message.role,
    content: [{ type: "text", text }, ...images],
  };
}

function openRaw(text: string) {
  const url = URL.createObjectURL(new Blob([text], { type: "text/markdown" }));
  window.open(url, "_blank");
}

function openDiff(files: { filename: string; body: string }[]) {
  const text = files.map((f) => `--- ${f.filename}\n${f.body}`).join("\n\n");
  openRaw(text);
}

function modelIcon(source: "local" | "cloud") {
  return source === "cloud"
    ? <Cloud size={12} />
    : <Cpu size={12} />;
}

interface MessageMarker {
  sessionId: string;
  index: number;
}

export function ChatView({
  status,
  session,
  gitStatus,
  onUpdateMessages,
  onEnsureSession,
  onStreamingChange,
  onSetPermission,
  onGitRefresh,
  gitBusy,
}: {
  status: StatusResponse | null;
  session: ChatSession | null;
  gitStatus: GitStatus | null;
  onUpdateMessages: (id: string, messages: StoredMessage[]) => void;
  onEnsureSession: () => Promise<ChatSession>;
  onStreamingChange: (id: string | null) => void;
  onSetPermission: (id: string, permission: PermissionMode) => void;
  onGitRefresh: () => Promise<void>;
  gitBusy: boolean;
}) {
  const { settings } = useSettings();
  const { toast } = useDialogs();
  const models = status?.models ?? [];
  const [model, setModel] = useState("");
  // Off by default (matches the daemon's default): a Qwen3-style hidden
  // <think> pass before the real answer, only useful for reasoning models.
  const [thinking, setThinking] = useState(false);
  const [input, setInput] = useState("");
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
  const [approvals, setApprovals] = useState<PendingApproval[]>([]);
  const [knowledgeStatus, setKnowledgeStatus] = useState<KnowledgeStatus | null>(null);
  const [presetState, setPresetState] = useState<WorkspacePresetList | null>(null);
  const [attachments, setAttachments] = useState<AttachmentDescriptor[]>([]);
  const [uploadingAttachment, setUploadingAttachment] = useState(false);
  const [dropActive, setDropActive] = useState(false);
  const [editingMessage, setEditingMessage] = useState<MessageMarker | null>(null);
  const [editingText, setEditingText] = useState("");
  const [copiedMessage, setCopiedMessage] = useState<MessageMarker | null>(null);
  const [gitBranches, setGitBranches] = useState<GitBranchInfo[]>([]);
  const [switchingBranch, setSwitchingBranch] = useState(false);
  const attachmentsRef = useRef<AttachmentDescriptor[]>([]);
  const attachmentReferenceRef = useRef<string | null>(null);
  const uploadVersionRef = useRef(0);
  const uploadActiveRef = useRef(false);
  const sessionIdRef = useRef<string | null>(session?.id ?? null);
  const dragDepthRef = useRef(0);
  const liveStepsRef = useRef<ToolStep[]>([]);

  sessionIdRef.current = session?.id ?? null;

  const effectiveModel = models.find((m) => m.id === model)?.id || models[0]?.id || "";
  const effectiveModelObj = models.find((m) => m.id === effectiveModel);

  useEffect(() => () => {
    sessionIdRef.current = null;
    uploadVersionRef.current += 1;
    const reference = attachmentReferenceRef.current;
    if (!reference) return;
    for (const attachment of attachmentsRef.current) {
      api.releaseOneAttachment(attachment.id, reference).catch(() => {});
    }
  }, []);

  // Manual compaction: typing "/compact" replaces the older half of the
  // chat with one model-written summary instead of sending it as a
  // message. Same routine `send()` below runs automatically when
  // settings.auto_compact is on and the history outgrows the model's
  // context window.
  const compact = async () => {
    if (!effectiveModel || !session || compacting) return;
    setInput("");
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
    if (
      (!text && attachments.length === 0) ||
      !effectiveModel ||
      sending ||
      uploadActiveRef.current
    ) return;
    if (
      attachments.some((attachment) => attachment.kind === "image") &&
      !effectiveModelObj?.vision_capable
    ) {
      setError("The selected model does not support image attachments");
      return;
    }
    if (text.toLowerCase() === "/compact") {
      await compact();
      return;
    }
    const target = session ?? (await onEnsureSession());
    setError(null);
    setInput("");
    setEditingMessage(null);
    setEditingText("");

    let baseMessages = target.messages;
    if (settings.auto_compact && shouldAutoCompact(baseMessages, effectiveModelObj?.context_length)) {
      try {
        baseMessages = await compactMessages(effectiveModel, baseMessages);
        onUpdateMessages(target.id, baseMessages);
      } catch {
        // Best-effort: send the uncompacted history rather than block the turn over it.
      }
    }
    const startMessages: StoredMessage[] = [
      ...baseMessages,
      {
        role: "user",
        content: text || "Please analyze the attached files.",
        attachments,
      },
    ];
    onUpdateMessages(target.id, startMessages);
    attachmentsRef.current = [];
    attachmentReferenceRef.current = null;
    setAttachments([]);

    setSending(true);
    setStreamingSessionId(target.id);
    onStreamingChange(target.id);
    const controller = new AbortController();
    abortRef.current = controller;
    const start = performance.now();
    let assistantText = "";
    liveStepsRef.current = [];
    try {
      const inferenceMessages = await Promise.all(
        startMessages.map(messageForInference),
      );
      await streamChat(
        effectiveModel,
        inferenceMessages,
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

  const uploadAttachments = async (
    files: FileList | readonly File[] | null,
  ) => {
    if (!files?.length || uploadActiveRef.current) return;
    const pendingFiles = Array.from(files);
    if (attachmentsRef.current.length + pendingFiles.length > 8) {
      setError("A message can contain at most 8 attachments");
      return;
    }
    const target = session ?? (await onEnsureSession());
    const reference = `chat:${target.id}`;
    const uploadVersion = ++uploadVersionRef.current;
    uploadActiveRef.current = true;
    attachmentReferenceRef.current = reference;
    setUploadingAttachment(true);
    setError(null);
    try {
      for (const file of pendingFiles) {
        if (uploadVersionRef.current !== uploadVersion) break;
        const descriptor = await api.uploadAttachment(
          reference,
          file.name,
          await blobAsDataUrl(file),
        );
        if (
          uploadVersionRef.current !== uploadVersion ||
          sessionIdRef.current !== target.id
        ) {
          await api.releaseOneAttachment(descriptor.id, reference).catch(() => {});
          break;
        }
        attachmentsRef.current = [...attachmentsRef.current, descriptor];
        setAttachments(attachmentsRef.current);
      }
    } catch (err) {
      if (uploadVersionRef.current === uploadVersion) {
        setError(err instanceof Error ? err.message : String(err));
      }
    } finally {
      if (uploadVersionRef.current === uploadVersion) {
        uploadActiveRef.current = false;
        setUploadingAttachment(false);
      }
    }
  };

  const removeAttachment = (attachment: AttachmentDescriptor) => {
    attachmentsRef.current = attachmentsRef.current.filter(
      (item) => item.id !== attachment.id,
    );
    setAttachments(attachmentsRef.current);
    const reference = attachmentReferenceRef.current;
    if (reference) {
      api
        .releaseOneAttachment(attachment.id, reference)
        .catch(() => {});
    }
  };

  const handleDragEnter = (event: DragEvent<HTMLDivElement>) => {
    if (!event.dataTransfer.types.includes("Files")) return;
    event.preventDefault();
    dragDepthRef.current += 1;
    setDropActive(true);
  };

  const handleDragOver = (event: DragEvent<HTMLDivElement>) => {
    if (!event.dataTransfer.types.includes("Files")) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
  };

  const handleDragLeave = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    dragDepthRef.current = Math.max(0, dragDepthRef.current - 1);
    if (dragDepthRef.current === 0) setDropActive(false);
  };

  const handleDrop = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    dragDepthRef.current = 0;
    setDropActive(false);
    if (!event.dataTransfer.files.length) return;
    void uploadAttachments(event.dataTransfer.files);
  };

  const copyMessage = async (index: number, content: string) => {
    if (!session) return;
    const marker = { sessionId: session.id, index };
    try {
      await navigator.clipboard.writeText(content);
      setCopiedMessage(marker);
      window.setTimeout(() => {
        setCopiedMessage((current) =>
          current?.sessionId === marker.sessionId && current.index === marker.index
            ? null
            : current,
        );
      }, 1500);
    } catch {
      setError("Could not copy the message");
    }
  };

  const beginEditMessage = (index: number, content: string) => {
    if (!session || sending) return;
    setEditingMessage({ sessionId: session.id, index });
    setEditingText(content);
  };

  const saveEditedMessage = () => {
    if (
      !editingMessage ||
      !session ||
      editingMessage.sessionId !== session.id ||
      sending ||
      !editingText.trim()
    ) return;
    onUpdateMessages(
      session.id,
      session.messages.map((message, index) => index === editingMessage.index
        ? { ...message, content: editingText.trim() }
        : message),
    );
    setEditingMessage(null);
    setEditingText("");
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
        setEditingMessage(null);
        setEditingText("");
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

  useEffect(() => {
    if (!session) {
      setApprovals([]);
      return;
    }
    const chatId = session.id;
    let cancelled = false;
    let timer: number | undefined;
    const poll = async () => {
      const result = await api.listApprovals(chatId).catch(() => null);
      if (cancelled) return;
      if (result) setApprovals(result.approvals);
      timer = window.setTimeout(poll, 400);
    };
    poll();
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [session?.id]);

  useEffect(() => {
    let cancelled = false;
    let timer: number | undefined;
    const poll = async () => {
      const result = await api.knowledgeStatus().catch(() => null);
      if (cancelled || !result) return;
      setKnowledgeStatus(result);
      if (result.running) timer = window.setTimeout(poll, 600);
    };
    poll();
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, []);

  useEffect(() => {
    api.listWorkspacePresets().then(setPresetState, () => {});
  }, [session?.projectId]);

  useEffect(() => {
    if (!session || !gitStatus?.is_repo) {
      setGitBranches([]);
      return;
    }
    let cancelled = false;
    api.gitWorkflowBranches(session.projectId).then(
      (result) => {
        if (!cancelled) setGitBranches(result.branches);
      },
      () => {
        if (!cancelled) setGitBranches([]);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [gitStatus?.branch, gitStatus?.is_repo, session?.projectId]);

  useEffect(() => {
    const currentReference = session?.id ? `chat:${session.id}` : null;
    dragDepthRef.current = 0;
    setDropActive(false);
    setEditingMessage(null);
    setEditingText("");
    setCopiedMessage(null);
    const staleReference = attachmentReferenceRef.current;
    if (!staleReference || staleReference === currentReference) return;
    uploadVersionRef.current += 1;
    uploadActiveRef.current = false;
    setUploadingAttachment(false);
    for (const attachment of attachmentsRef.current) {
      api
        .releaseOneAttachment(attachment.id, staleReference)
        .catch(() => {});
    }
    attachmentsRef.current = [];
    attachmentReferenceRef.current = null;
    setAttachments([]);
  }, [session?.id]);

  const toggleKnowledgeIndex = async () => {
    setError(null);
    try {
      if (knowledgeStatus?.running) {
        await api.cancelKnowledgeIndex();
      } else {
        await api.startKnowledgeIndex();
      }
      setKnowledgeStatus(await api.knowledgeStatus());
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const applyWorkspacePreset = async (presetId: string) => {
    if (!presetId || !presetState) return;
    const preset = presetState.presets.find((item) => item.id === presetId);
    if (!preset) return;
    const target = session ?? (await onEnsureSession());
    setError(null);
    try {
      await api.applyWorkspacePreset(
        preset.id,
        preset.revision,
        target.projectId,
      );
      onSetPermission(target.id, preset.permission);
      if (preset.preferred_model) setModel(preset.preferred_model);
      setPresetState(await api.listWorkspacePresets());
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const switchBranch = async (name: string) => {
    if (
      !session ||
      !name ||
      name === gitStatus?.branch ||
      gitBusy ||
      sending
    ) return;
    setSwitchingBranch(true);
    setError(null);
    try {
      await api.gitWorkflowSwitchBranch(session.projectId, name);
      await onGitRefresh();
      const result = await api.gitWorkflowBranches(session.projectId);
      setGitBranches(result.branches);
      toast(`Switched to ${name}`, "success");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSwitchingBranch(false);
    }
  };

  const decideApproval = async (approval: PendingApproval, decision: ApprovalDecision) => {
    setApprovals((current) => current.filter((item) => item.id !== approval.id));
    try {
      await api.resolveApproval(approval.id, decision);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const isStreamingThisSession = sending && streamingSessionId === session?.id;
  const editingIndex = editingMessage && editingMessage.sessionId === session?.id
    ? editingMessage.index
    : null;
  const copiedIndex = copiedMessage && copiedMessage.sessionId === session?.id
    ? copiedMessage.index
    : null;
  const messages = session?.messages ?? [];
  const presetBinding = session
    ? presetState?.bindings[session.projectId]
    : undefined;
  const activePresetId = presetState?.presets.some(
    (preset) =>
      preset.id === presetBinding?.preset_id &&
      preset.revision === presetBinding.preset_revision,
  )
    ? presetBinding?.preset_id ?? ""
    : "";
  const modelGroups = groupBySection(models, (item) => item.id).map(
    (group) => ({
      section: group.section,
      options: group.items.map((item) => ({
        value: item.id,
        label: modelName(item.id),
        icon: modelIcon(item.source),
      })),
    }),
  );

  // Secondary controls move into overflow on narrow composers.
  const renderConfigChips = () => (
    <>
      {effectiveModelObj && (
        <span className="chip-select chip-static" title="Trained context window, from the model's GGUF metadata">
          {formatContextLength(effectiveModelObj.context_length)}
        </span>
      )}
      <button
        type="button"
        className={
          knowledgeStatus?.running
            ? "chip-select chip-toggle chip-toggle-on"
            : "chip-select chip-toggle"
        }
        title={
          knowledgeStatus
            ? `${knowledgeStatus.snapshot.files} indexed workspace files`
            : "Index workspace files for retrieval"
        }
        onClick={() => void toggleKnowledgeIndex()}
      >
        <Database size={12} />
        {knowledgeStatus?.running ? "Cancel indexing" : "Index knowledge"}
      </button>
      {presetState && presetState.presets.length > 0 && (
        <Select
          triggerClassName="chip-select"
          value={activePresetId}
          onChange={(value) => void applyWorkspacePreset(value)}
          options={[
            { value: "", label: "Workspace preset" },
            ...presetState.presets.map((preset) => ({
              value: preset.id,
              label: preset.name,
            })),
          ]}
        />
      )}
    </>
  );

  const renderChatModeControls = () => (
    <div className="composer-mode-group">
      <Select
        triggerClassName={
          session?.permission === "full-access"
            ? "chip-select chip-toggle chip-toggle-on"
            : "chip-select chip-toggle"
        }
        icon={
          session?.permission === "full-access" ? (
            <Unlock size={12} />
          ) : session?.permission === "ask" ? (
            <ShieldQuestion size={12} />
          ) : (
            <Lock size={12} />
          )
        }
        value={session?.permission ?? "read-only"}
        onChange={(permission) => session && onSetPermission(session.id, permission as PermissionMode)}
        options={[
          { value: "read-only", label: "Read only" },
          { value: "ask", label: "Ask before changes" },
          { value: "full-access", label: "Full access" },
        ]}
      />
      <button
        type="button"
        className={thinking
          ? "chip-select chip-toggle chip-toggle-on"
          : "chip-select chip-toggle"}
        title="Let reasoning models think before answering"
        onClick={() => setThinking((current) => !current)}
      >
        <Brain size={12} /> {thinking ? "Thinking" : "Think"}
      </button>
    </div>
  );

  const renderMessageActions = (message: StoredMessage, index: number) => (
    <div className="message-actions">
      <button
        type="button"
        title="Copy message"
        aria-label="Copy message"
        onClick={() => void copyMessage(index, message.content)}
      >
        {copiedIndex === index ? <Check size={13} /> : <Copy size={13} />}
      </button>
      <button
        type="button"
        title="Edit message"
        aria-label="Edit message"
        disabled={sending}
        onClick={() => beginEditMessage(index, message.content)}
      >
        <Pencil size={13} />
      </button>
    </div>
  );

  const renderMessageEditor = () => (
    <div className="message-editor">
      <textarea
        value={editingText}
        onChange={(event) => setEditingText(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Escape") setEditingMessage(null);
          if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
            event.preventDefault();
            saveEditedMessage();
          }
        }}
        autoFocus
      />
      <div className="message-editor-actions">
        <button
          type="button"
          className="btn btn-ghost"
          onClick={() => setEditingMessage(null)}
        >
          Cancel
        </button>
        <button
          type="button"
          className="btn btn-accent"
          disabled={!editingText.trim()}
          onClick={saveEditedMessage}
        >
          Save
        </button>
      </div>
    </div>
  );

  return (
    <div
      className={dropActive ? "thread-col thread-col-drop-active" : "thread-col"}
      onDragEnter={handleDragEnter}
      onDragOver={handleDragOver}
      onDragLeave={handleDragLeave}
      onDrop={handleDrop}
    >
      {dropActive && (
        <div className="chat-drop-overlay">
          <Paperclip size={20} />
          <span>Drop files to attach</span>
        </div>
      )}
      <div className="chat-log">
        {messages.length === 0 && !isStreamingThisSession && (
          <div className="chat-empty">Send a message to start a new chat.</div>
        )}
        {messages.map((m, i) =>
          m.role === "user" ? (
            <div key={i} className="turn-user">
              <div className="chat-bubble chat-bubble-user">
                {editingIndex === i
                  ? renderMessageEditor()
                  : <Markdown>{m.content}</Markdown>}
                {m.attachments && m.attachments.length > 0 && (
                  <div className="message-attachments">
                    {m.attachments.map((attachment) =>
                      attachment.kind === "image" ? (
                        <img
                          key={attachment.id}
                          src={api.attachmentUrl(attachment.id)}
                          alt={attachment.filename}
                        />
                      ) : (
                        <span className="attachment-chip" key={attachment.id}>
                          {attachment.filename}
                        </span>
                      ),
                    )}
                  </div>
                )}
              </div>
              {editingIndex !== i && renderMessageActions(m, i)}
            </div>
          ) : (
            <div key={i} className="turn-assistant">
              {editingIndex === i
                ? renderMessageEditor()
                : stripCodeBlocks(m.content) && (
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
              {editingIndex !== i && renderMessageActions(m, i)}
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
        {approvals[0] && (
          <div className="approval-card" role="alert">
            <div className="approval-card-head">
              <span className="approval-card-title">
                <ShieldQuestion size={14} /> Approval required
              </span>
              <span className="approval-risk">{approvals[0].risk.replaceAll("_", " ")}</span>
            </div>
            <div className="approval-tool mono">{approvals[0].tool_name}</div>
            <div className="approval-target mono">{approvals[0].target}</div>
            <pre className="approval-args mono">{JSON.stringify(approvals[0].arguments, null, 2)}</pre>
            <div className="approval-actions">
              <button
                type="button"
                className="btn btn-ghost"
                onClick={() => decideApproval(approvals[0], "deny")}
              >
                Deny
              </button>
              <button
                type="button"
                className="btn btn-ghost"
                onClick={() => decideApproval(approvals[0], "allow_once")}
              >
                Allow once
              </button>
              <button
                type="button"
                className="btn btn-primary"
                onClick={() => decideApproval(approvals[0], "allow_for_chat")}
              >
                Allow for this chat
              </button>
            </div>
          </div>
        )}
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
          {attachments.length > 0 && (
            <div className="composer-attachments">
              {attachments.map((attachment) => (
                <span className="attachment-chip" key={attachment.id}>
                  {attachment.filename}
                  <button
                    type="button"
                    title={`Remove ${attachment.filename}`}
                    onClick={() => removeAttachment(attachment)}
                  >
                    <X size={11} />
                  </button>
                </span>
              ))}
            </div>
          )}
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
              // Enter sends; Ctrl/Cmd+Enter is the alternate.
              const sends = settings.send_on_enter
                ? !e.shiftKey
                : e.metaKey || e.ctrlKey;
              if (sends) {
                e.preventDefault();
                send();
              }
            }}
          />
          <div className="composer-toolbar">
            <div className="composer-chips">
              <label
                className="chip-select composer-attach composer-icon-control"
                title="Attach an image, PDF, or text file"
                aria-label="Attach files"
              >
                <Paperclip
                  size={14}
                  className={uploadingAttachment ? "spin-icon" : undefined}
                />
                <input
                  type="file"
                  multiple
                  disabled={uploadingAttachment}
                  accept="image/png,image/jpeg,image/webp,image/gif,application/pdf,text/plain,text/markdown,application/json"
                  onChange={(event) => {
                    void uploadAttachments(event.target.files);
                    event.target.value = "";
                  }}
                />
              </label>
              <Select
                triggerClassName="chip-select chip-select-model"
                icon={effectiveModelObj
                  ? modelIcon(effectiveModelObj.source)
                  : <Cpu size={12} />}
                value={effectiveModel}
                onChange={setModel}
                options={models.length === 0 ? [{ value: "", label: "no models" }] : undefined}
                groups={models.length === 0 ? undefined : modelGroups}
              />
              {renderChatModeControls()}
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
                disabled={
                  !effectiveModel ||
                  (!input.trim() && attachments.length === 0) ||
                  sending ||
                  uploadingAttachment ||
                  compacting
                }
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
            <Select
              triggerClassName="composer-git-branch-select mono"
              value={gitStatus.branch ?? ""}
              disabled={
                switchingBranch ||
                gitBusy ||
                sending ||
                gitBranches.length === 0
              }
              onChange={(name) => void switchBranch(name)}
              options={[
                ...(gitStatus.branch
                  ? []
                  : [{ value: "", label: "detached HEAD" }]),
                ...gitBranches.map((branch) => ({
                  value: branch.name,
                  label: `${branch.name}${
                    branch.current && gitStatus.dirty ? " •" : ""
                  }`,
                })),
              ]}
            />
          </div>
        )}
      </div>
    </div>
  );
}
