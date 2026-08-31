import { useEffect, useState } from "react";
import { api, type ChatDto } from "../api";
import { titleFromMessage, type ChatSession, type StoredMessage } from "../sessions";
import { useSettings } from "../settings";

function fromDto(c: ChatDto): ChatSession {
  return {
    id: c.id,
    title: c.title,
    createdAt: c.created_at,
    updatedAt: c.updated_at,
    projectId: c.project_id,
    permission:
      c.permission === "full-access" ? "full-access" : c.permission === "ask" ? "ask" : "read-only",
    systemPrompt: c.system_prompt ?? "",
    messages: (c.messages as StoredMessage[] | null) ?? [],
  };
}

// Owns the session list + active selection, daemon-persisted
// (~/.kiralm/chats.json, see src/chats.rs) so chat history survives a
// reload/relaunch instead of living only in this browser's
// localStorage. Fetched once on mount, same pattern as useProjects.
export function useSessions() {
  const [sessions, setSessions] = useState<ChatSession[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const { settings } = useSettings();

  const refreshSessions = async () => {
    const { chats } = await api
      .listChats()
    const loaded = chats.map(fromDto);
    setSessions(loaded);
    setActiveId((cur) => cur && loaded.some((chat) => chat.id === cur) ? cur : loaded[0]?.id ?? null);
  };

  useEffect(() => {
    refreshSessions().catch(() => {});
  }, []);

  const createSession = async (projectId: string) => {
    const dto = await api.createChat(projectId, settings.default_permission);
    const session = fromDto(dto);
    setSessions((prev) => [session, ...prev]);
    setActiveId(session.id);
    api.touchProject(projectId).catch(() => {});
    return session;
  };

  const setPermission = (id: string, permission: ChatSession["permission"]) => {
    setSessions((prev) => prev.map((s) => (s.id === id ? { ...s, permission } : s)));
    api.updateChat(id, { permission }).catch(() => {});
  };

  const setSystemPrompt = async (id: string, systemPrompt: string) => {
    await api.updateChat(id, { system_prompt: systemPrompt });
    setSessions((prev) => prev.map((s) => (s.id === id ? { ...s, systemPrompt } : s)));
  };

  const renameSession = (id: string, title: string) => {
    setSessions((prev) => prev.map((s) => (s.id === id ? { ...s, title } : s)));
    api.updateChat(id, { title }).catch(() => {});
  };

  const deleteSession = (id: string) => {
    setSessions((prev) => prev.filter((s) => s.id !== id));
    setActiveId((cur) => (cur === id ? null : cur));
    api.deleteChat(id).catch(() => {});
  };

  // Deleting a workspace takes its chats with it server-side too (see
  // delete_project in internal.rs) -- this just keeps this tab's own
  // view in sync with that.
  const deleteSessionsByProject = (projectId: string) => {
    setSessions((prev) => prev.filter((s) => s.projectId !== projectId));
    setActiveId((cur) => (sessions.find((s) => s.id === cur)?.projectId === projectId ? null : cur));
  };

  const setMessages = (id: string, messages: StoredMessage[]) => {
    setSessions((prev) =>
      prev.map((s) => {
        if (s.id !== id) return s;
        return {
          ...s,
          messages,
          updatedAt: new Date().toISOString(),
          title: s.title !== "New chat" ? s.title : titleFromMessage(messages[0]?.content ?? ""),
        };
      }),
    );
    const current = sessions.find((s) => s.id === id);
    const title = current?.title;
    const nextTitle = title && title !== "New chat" ? title : titleFromMessage(messages[0]?.content ?? "");
    api.updateChat(id, { messages, title: nextTitle }).catch(() => {});
    if (current) api.touchProject(current.projectId).catch(() => {});
  };

  const active = sessions.find((s) => s.id === activeId) ?? null;

  return {
    sessions,
    active,
    activeId,
    setActiveId,
    createSession,
    setMessages,
    setPermission,
    setSystemPrompt,
    renameSession,
    deleteSession,
    deleteSessionsByProject,
    refreshSessions,
  };
}
