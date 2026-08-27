// Typed client for KiraLM's internal (/api/*) and OpenAI-compatible (/v1/*)
// routes. Same-origin: the daemon serves this SPA alongside the API, so no
// base URL config is needed. Plain relative fetches everywhere.

export type VerdictTier =
  | "FITS_FAST"
  | "USABLE_DEGRADED"
  | "NOT_RECOMMENDED"
  | "UNSUPPORTED_ARCHITECTURE";

export interface ModelStatus {
  id: string;
  aliases: string[];
  source: "local" | "cloud";
  size_bytes: number;
  quant: string;
  hot: boolean;
  context_length: number | null;
  loaded: boolean;
  rss_bytes: number | null;
  verdict: string | null;
  verdict_reason: string | null;
  verdict_tier: VerdictTier | null;
}

export interface StatusResponse {
  models: ModelStatus[];
  ram: {
    total_bytes: number;
    available_bytes: number;
    headroom_bytes: number;
  };
  config: {
    ram_headroom_gb: number;
    model_dir: string;
  };
}

// The static, immutable-once-pulled sidecar (state.rs's ModelManifest)
// -- no hot/verdict/last-used here, those are runtime state, see
// ModelDetail.state below.
export interface ModelManifest {
  canonical_id: string;
  filename: string;
  sha256: string;
  size_bytes: number;
  architecture: string;
  is_moe: boolean;
  expert_count: { total: number; active: number } | null;
  context_length_trained: number;
  vision_capable: boolean;
  source_ref: string;
  pulled_at: string;
}

// GET /api/models/:id's real shape: live state always present,
// manifest null if the sidecar failed to load.
export interface ModelDetail {
  state: ModelStatus;
  manifest: ModelManifest | null;
}

export interface ApiError {
  error: { message: string; type?: string; param?: string; code?: string };
}

export interface CloudStatus {
  logged_in: boolean;
  device: string | null;
}

export interface AppSettings {
  default_permission: "read-only" | "full-access";
  send_on_enter: boolean;
  notify_on_finish: boolean;
  default_hot_on_pull: boolean;
  line_height: "compact" | "normal" | "relaxed";
  // "default" (bundled Open Sans) or a real system font family name.
  font: string;
  sidebar_collapsed: boolean;
  // Project ids whose chat list is collapsed in the sidebar.
  collapsed_workspaces: string[];
}

// `~/.kiralm/chats.json`'s wire shape (daemon-persisted chat history,
// see sessions.ts's ChatSession for the app-side camelCase shape).
// `messages` is opaque here -- the daemon stores/returns it verbatim.
export interface ChatDto {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  project_id: string;
  permission: string;
  messages: unknown;
}

export interface TunnelStatus {
  enabled: boolean;
  hostname?: string;
  cf_tunnel_id?: string;
  running?: boolean;
  connectivity_confirmed?: boolean;
}

export interface Project {
  id: string;
  name: string;
  path: string;
  created_at: string;
  last_active_at: string;
}

export interface GitFileStatus {
  path: string;
  status: string;
}

export interface GitStatus {
  is_repo: boolean;
  branch: string | null;
  dirty: boolean;
  files: GitFileStatus[];
}

export interface GitCommitResult {
  commit: string;
  branch: string;
  pushed: boolean;
}

export interface CloudLoginStart {
  device_code: string;
  user_code: string;
  verification_uri: string;
  verification_uri_complete: string | null;
  expires_in: number;
}

async function asJson<T>(res: Response): Promise<T> {
  const body = await res.json().catch(() => null);
  if (!res.ok) {
    const message =
      (body as ApiError | null)?.error?.message ?? `${res.status} ${res.statusText}`;
    throw new Error(message);
  }
  return body as T;
}

export const api = {
  status: () => fetch("/api/status").then((r) => asJson<StatusResponse>(r)),

  model: (id: string) =>
    fetch(`/api/models/${encodeURIComponent(id)}`).then((r) => asJson<ModelDetail>(r)),

  pull: (ref: string) =>
    fetch("/api/models/pull", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ ref }),
    }).then((r) => asJson<{ id: string }>(r)),

  setHot: (id: string, on: boolean, force = false) =>
    fetch(`/api/models/${encodeURIComponent(id)}/hot`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ on, force }),
    }).then((r) => asJson<void>(r)),

  remove: (id: string) =>
    fetch(`/api/models/${encodeURIComponent(id)}`, { method: "DELETE" }).then((r) =>
      asJson<void>(r),
    ),

  pullProgressUrl: (id: string) => `/api/models/${encodeURIComponent(id)}/pull`,

  cloudStatus: () => fetch("/api/cloud/status").then((r) => asJson<CloudStatus>(r)),

  cloudLogin: () =>
    fetch("/api/cloud/login", { method: "POST" }).then((r) => asJson<CloudLoginStart>(r)),

  cloudLogout: () => fetch("/api/cloud/logout", { method: "POST" }).then((r) => asJson<void>(r)),

  cloudLoginProgressUrl: (deviceCode: string) =>
    `/api/cloud/login/progress?device_code=${encodeURIComponent(deviceCode)}`,

  // Writes ram_headroom_gb/model_dir into config.toml. Takes effect on
  // next daemon restart, not live -- config.rs loads it once at startup.
  updateConfig: (ramHeadroomGb: number | null, modelDir: string) =>
    fetch("/api/config", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ ram_headroom_gb: ramHeadroomGb, model_dir: modelDir }),
    }).then((r) => asJson<{ restart_required: boolean }>(r)),

  tunnelStatus: () => fetch("/api/tunnel/status").then((r) => asJson<TunnelStatus>(r)),

  tunnelEnable: () =>
    fetch("/api/tunnel/enable", { method: "POST" }).then((r) => asJson<{ hostname: string }>(r)),

  tunnelDisable: () => fetch("/api/tunnel/disable", { method: "POST" }).then((r) => asJson<void>(r)),

  getSettings: () => fetch("/api/settings").then((r) => asJson<AppSettings>(r)),

  putSettings: (settings: AppSettings) =>
    fetch("/api/settings", {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(settings),
    }).then((r) => asJson<AppSettings>(r)),

  listProjects: () => fetch("/api/projects").then((r) => asJson<{ projects: Project[] }>(r)),

  createProject: (path: string, name?: string) =>
    fetch("/api/projects", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ path, name }),
    }).then((r) => asJson<Project>(r)),

  deleteProject: (id: string) =>
    fetch(`/api/projects/${encodeURIComponent(id)}`, { method: "DELETE" }).then((r) => asJson<void>(r)),

  // Chat history, daemon-persisted (~/.kiralm/chats.json) so it
  // survives a reload/relaunch instead of living only in localStorage.
  listChats: () => fetch("/api/chats").then((r) => asJson<{ chats: ChatDto[] }>(r)),

  createChat: (projectId: string, permission: string) =>
    fetch("/api/chats", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ project_id: projectId, permission }),
    }).then((r) => asJson<ChatDto>(r)),

  updateChat: (id: string, patch: { title?: string; permission?: string; messages?: unknown }) =>
    fetch(`/api/chats/${encodeURIComponent(id)}`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(patch),
    }).then((r) => asJson<ChatDto>(r)),

  deleteChat: (id: string) =>
    fetch(`/api/chats/${encodeURIComponent(id)}`, { method: "DELETE" }).then((r) => asJson<void>(r)),

  // Real directory listing for the in-app folder browser (see
  // FolderBrowser) -- subdirectories only, home dir if `path` is
  // omitted. Not a native OS dialog.
  listDir: (path?: string) =>
    fetch(`/api/fs/list${path ? `?path=${encodeURIComponent(path)}` : ""}`).then((r) =>
      asJson<{ path: string; parent: string | null; entries: { name: string; path: string }[] }>(r),
    ),

  // Real `git clone <url> <path>`, then registers the clone as a
  // project. Throws with the real git error (e.g. "already exists").
  cloneProject: (url: string, path: string, name?: string) =>
    fetch("/api/projects/clone", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ url, path, name }),
    }).then((r) => asJson<Project>(r)),

  touchProject: (id: string) =>
    fetch(`/api/projects/${encodeURIComponent(id)}`, { method: "PATCH" }).then((r) => asJson<void>(r)),

  // Real git status for a project's directory (branch + dirty files),
  // shelled out to the system git binary daemon-side. `is_repo: false`
  // for a project that isn't a git checkout, not an error.
  gitStatus: (projectId: string) =>
    fetch(`/api/projects/${encodeURIComponent(projectId)}/git/status`).then((r) => asJson<GitStatus>(r)),

  // Real `git add -A && git commit && git push` against the project's
  // directory. Throws with the real git error (e.g. "nothing to
  // commit", or a commit that landed but failed to push).
  gitCommit: (projectId: string, message?: string) =>
    fetch(`/api/projects/${encodeURIComponent(projectId)}/git/commit`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ message }),
    }).then((r) => asJson<GitCommitResult>(r)),

  // Server-side generation state for a chat session: buffered-so-far
  // text + whether it's still running,
  // independent of any open connection. Always 200, empty/not-running
  // for a session that never generated anything.
  generation: (sessionId: string) =>
    fetch(`/api/sessions/${encodeURIComponent(sessionId)}/generation`).then((r) =>
      asJson<GenerationState>(r),
    ),

  // Session ids with generation currently in flight, for the sidebar's
  // status dots to reflect real background state for every session, not
  // just the focused one.
  runningGenerations: () =>
    fetch("/api/generations").then((r) => asJson<{ running: string[] }>(r)),

  // Actually stops server-side generation (aborts the daemon's upstream
  // connection), not just this tab's own view of it.
  stopGeneration: (sessionId: string) =>
    fetch(`/api/sessions/${encodeURIComponent(sessionId)}/generation`, { method: "DELETE" }).then(
      (r) => asJson<void>(r),
    ),
};

export interface GenerationState {
  model: string;
  text: string;
  running: boolean;
  error: string | null;
  steps: ToolStep[];
}

export interface ChatMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

// One real tool call the daemon made while producing a turn -- the
// step data behind "Worked for Xm Ys".
export interface ToolStep {
  name: string;
  args_summary: string;
  result_summary: string;
  ok: boolean;
  // Real before/after content for a successful write_file call (see
  // src/tools.rs's FileEdit), present only on that step -- what the
  // Diff tab's real per-turn diff is built from.
  file_edit?: { path: string; before: string | null; after: string } | null;
}

// Streams an OpenAI-shaped SSE response, calling onToken per delta. The
// daemon tracks generation server-side under `sessionId` (independent of
// this connection, see api.generation/runningGenerations above), so
// aborting or navigating away here doesn't stop the model generating.
// `projectId`/`permission` scope the daemon's tool-calling loop (sandbox
// root + read-only/full-access) to this session; omitted for no active
// project, which just gets the plain passthrough with no tool access.
export async function streamChat(
  model: string,
  messages: ChatMessage[],
  sessionId: string,
  onToken: (text: string) => void,
  signal?: AbortSignal,
  projectId?: string,
  permission?: "read-only" | "full-access",
  onSteps?: (steps: ToolStep[]) => void,
  // llama-server-specific: "thinking" (a Qwen3-style hidden <think>
  // pass before the real answer) is off by default so tool-loop turns
  // don't burn max_tokens on it. The composer's "Thinking" toggle flips this.
  enableThinking?: boolean,
): Promise<void> {
  const headers: Record<string, string> = {
    "content-type": "application/json",
    "x-kiralm-session-id": sessionId,
  };
  if (projectId) headers["x-kiralm-project-id"] = projectId;
  if (permission) headers["x-kiralm-permission-mode"] = permission;

  const res = await fetch("/v1/chat/completions", {
    method: "POST",
    headers,
    body: JSON.stringify({
      model,
      messages,
      stream: true,
      ...(enableThinking ? { chat_template_kwargs: { enable_thinking: true } } : {}),
    }),
    signal,
  });
  if (!res.ok || !res.body) {
    const body = await res.json().catch(() => null);
    throw new Error((body as ApiError | null)?.error?.message ?? `${res.status} ${res.statusText}`);
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    const lines = buf.split("\n");
    buf = lines.pop() ?? "";
    for (const line of lines) {
      const data = line.startsWith("data:") ? line.slice(5).trim() : "";
      if (!data || data === "[DONE]") continue;
      try {
        const chunk = JSON.parse(data);
        const delta: string | undefined = chunk.choices?.[0]?.delta?.content;
        if (delta) onToken(delta);
        // Real tool-call step log, only present on the tool-loop path's
        // final frame (see run_tool_loop's synthetic_stream_response);
        // absent (and harmlessly ignored) for the plain passthrough.
        if (chunk.kiralm_tool_steps) onSteps?.(chunk.kiralm_tool_steps as ToolStep[]);
      } catch {
        // partial/malformed line, skip since the next chunk carries the rest
      }
    }
  }
}
