// Chat session shape + small helpers. Persistence itself lives
// server-side (~/.kiralm/chats.json, see useSessions.ts), not here.
import type {
  AttachmentDescriptor,
  PermissionMode,
  ToolStep,
} from "./api";

export interface StoredMessage {
  role: "system" | "user" | "assistant";
  content: string;
  attachments?: AttachmentDescriptor[];
  // Real elapsed generation time for an assistant turn, set once
  // streaming finishes. Stripped before sending back to the API.
  durationMs?: number;
  // Real tool calls the daemon made while producing this turn, in
  // order -- what "Worked for Xm Ys" expands to.
  steps?: ToolStep[];
}

export interface ChatSession {
  id: string;
  title: string;
  createdAt: string;
  updatedAt: string;
  messages: StoredMessage[];
  // Real project (workspace/repo root) this session is scoped to.
  projectId: string;
  // Real per-session tool-access mode (the composer's "Full access"
  // chip): "read-only" until explicitly flipped, same default a fresh
  // session gets server-side when the header is simply absent.
  permission: PermissionMode;
  // Optional per-chat instructions, composed below global and
  // workspace instructions by the daemon on every agent turn.
  systemPrompt: string;
}

// Chat title and sidebar preview are single-line truncated labels, so
// full markdown rendering doesn't fit -- strip the syntax down to plain
// text instead of leaking raw "**bold**"/"- item" markers into them.
function stripMarkdownSyntax(text: string): string {
  return text
    .replace(/```[^\n`]*\n?([\s\S]*?)```/g, "$1")
    .replace(/`([^`]+)`/g, "$1")
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/^#{1,6}\s+/gm, "")
    .replace(/^>\s?/gm, "")
    .replace(/^\s*[-*+]\s+/gm, "")
    .replace(/^\s*\d+\.\s+/gm, "")
    .replace(/(\*\*|__)(.*?)\1/g, "$2")
    .replace(/(\*|_)(.*?)\1/g, "$2");
}

export function titleFromMessage(text: string): string {
  const oneLine = stripMarkdownSyntax(text).trim().replace(/\s+/g, " ");
  return oneLine.length > 48 ? `${oneLine.slice(0, 48)}…` : oneLine || "New chat";
}

export function previewOf(session: ChatSession): string {
  const last = session.messages[session.messages.length - 1];
  if (!last) return "No messages yet";
  return titleFromMessage(last.content || "…");
}
