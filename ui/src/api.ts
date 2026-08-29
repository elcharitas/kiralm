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
    cache_quota_gb: number;
    optimization_profile: "balanced" | "throughput" | "latency" | "memory";
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
  default_permission: PermissionMode;
  send_on_enter: boolean;
  notify_on_finish: boolean;
  default_hot_on_pull: boolean;
  line_height: "compact" | "normal" | "relaxed";
  // "default" (bundled Open Sans) or a real system font family name.
  font: string;
  sidebar_collapsed: boolean;
  // Project ids whose chat list is collapsed in the sidebar.
  collapsed_workspaces: string[];
  // Whether a long chat gets its older messages summarized
  // automatically instead of always resending full history.
  auto_compact: boolean;
}

export interface KnowledgeStatus {
  snapshot: { files: number; chunks: number; content_bytes: number };
  running: boolean;
  recovered_corrupt_metadata: string | null;
}

export interface KnowledgeHit {
  score: number;
  content: string;
  source: {
    root: string;
    path: string;
    start_line: number;
    end_line: number;
  };
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

export type PermissionMode = "read-only" | "ask" | "full-access";

export interface PendingApproval {
  id: string;
  chat_id: string;
  tool_name: string;
  risk:
    | "read"
    | "write"
    | "process_execution"
    | "network_access"
    | "git_mutation"
    | "destructive_filesystem"
    | "external_mcp";
  target: string;
  arguments: unknown;
  requested_at: string;
  expires_at: string;
}

export type ApprovalDecision = "allow_once" | "allow_for_chat" | "deny";

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

export interface GitLogEntry {
  hash: string;
  short_hash: string;
  message: string;
  author: string;
  // Strict ISO 8601 (git's %aI), parseable with plain `new Date()`.
  date: string;
}

export interface GitCommitResult {
  hash: string;
  short_hash: string;
  summary: string;
}

// `~/.kiralm/skills/<name>/SKILL.md`'s wire shape -- global, no
// per-workspace scoping. `enabled` is KiraLM's own state (skills.json),
// not part of the SKILL.md format itself.
export interface Skill {
  name: string;
  description: string;
  allowed_tools?: string[] | null;
  license?: string | null;
  metadata?: unknown;
  body: string;
  enabled: boolean;
}

// `~/.kiralm/mcp_servers.json`'s wire shape merged with the live
// in-memory connection status -- `connected`/`tool_count`/`error`
// reflect the running child process, not anything persisted.
export interface McpServer {
  name: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  enabled: boolean;
  connected: boolean;
  tool_count: number;
  error?: string | null;
}

// A plugin entry as marketplace.json itself lists it -- no
// installed-state here, that's only known per-marketplace (see
// MarketplacePlugin below).
export interface MarketplacePluginRef {
  name: string;
  source: string;
  description?: string | null;
}

// `.claude-plugin/marketplace.json`'s wire shape, as cloned under
// `~/.kiralm/marketplaces/<slug>/`.
export interface Marketplace {
  slug: string;
  name: string;
  owner?: unknown;
  plugins: MarketplacePluginRef[];
}

// GET .../plugins's per-plugin view -- `installed` reflects real
// tagged skills/mcp servers, not marketplace.json alone.
export interface MarketplacePlugin extends MarketplacePluginRef {
  installed: boolean;
}

export interface PluginInstallResult {
  skills: string[];
  mcp_servers: string[];
  warnings: string[];
}

export interface PluginUninstallResult {
  skills: string[];
  mcp_servers: string[];
}

// `~/.kiralm/agent_plugins.json`'s wire shape -- one entry per
// installed Agent Plugins 1.0 plugin (agent-plugins.org), sourced from
// a local path or a git URL pointing straight at the plugin's root.
export interface AgentPlugin {
  name: string;
  source: string;
  plugin_ref: { marketplace: string; plugin: string };
  version?: string | null;
  description?: string | null;
  author?: unknown;
  homepage?: string | null;
  repository?: string | null;
  license?: string | null;
  keywords: string[];
  extensions?: unknown;
  installed_at: string;
}

export interface AgentPluginInstallResult {
  plugin: AgentPlugin;
  skills: string[];
  mcp_servers: string[];
  warnings: string[];
}

// GET /api/plugins/catalog's per-row shape: kiralm-web's cloud catalog
// (GET /api/plugins) plus a real `installed` flag re-derived from the
// matching backend's own live state -- not a separate install registry.
export type PluginSourceKind = "claude_marketplace" | "agent_plugin" | "dsh";

export interface CatalogPlugin {
  id: string;
  name: string;
  description: string | null;
  source_kind: PluginSourceKind;
  source: string;
  plugin_name: string | null;
  enabled: boolean;
  sort_order: number;
  installed: boolean;
}

// GET /api/models/catalog's per-row shape: kiralm-web's cloud catalog
// local_pull rows, already synced to disk -- replaces the hardcoded
// "pick a model to pull" list ModelsView.tsx used to ship with.
export interface CloudPullSuggestion {
  id: string;
  name: string;
  ref: string;
  size_bytes: number | null;
  description: string | null;
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
    // Two real shapes in the wild: the daemon's own routes return a bare
    // `{error: "..."}` string, kiralm-web/OpenAI-style routes return the
    // richer `{error: {message: "..."}}`.
    const rawError = (body as { error?: unknown } | null)?.error;
    const message =
      typeof rawError === "string"
        ? rawError
        : (rawError as { message?: string } | undefined)?.message ?? `${res.status} ${res.statusText}`;
    throw new Error(message);
  }
  return body as T;
}

export function subscribeSse(
  path: string,
  onMessage: (data: string) => void,
  onError: () => void,
) {
  const abort = new AbortController();
  fetch(path, { signal: abort.signal })
    .then(async (response) => {
      if (!response.ok || !response.body) throw new Error(`${response.status} ${response.statusText}`);
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let boundary = buffer.indexOf("\n\n");
        while (boundary !== -1) {
          const event = buffer.slice(0, boundary);
          buffer = buffer.slice(boundary + 2);
          for (const line of event.split("\n")) {
            if (line.startsWith("data:")) onMessage(line.slice(5).trim());
          }
          boundary = buffer.indexOf("\n\n");
        }
      }
    })
    .catch((error) => {
      if (!(error instanceof DOMException && error.name === "AbortError")) onError();
    });
  return () => abort.abort();
}

export const api = {
  status: () => fetch("/api/status").then((r) => asJson<StatusResponse>(r)),

  model: (id: string) =>
    fetch(`/api/models/${encodeURIComponent(id)}`).then((r) => asJson<ModelDetail>(r)),

  modelsCatalog: () => fetch("/api/models/catalog").then((r) => asJson<{ data: CloudPullSuggestion[] }>(r)),

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

  // Writes runtime keys into config.toml for the next restart.
  updateConfig: (
    ramHeadroomGb: number | null,
    modelDir: string,
    cacheQuotaGb: number,
    optimizationProfile: StatusResponse["config"]["optimization_profile"],
  ) =>
    fetch("/api/config", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        ram_headroom_gb: ramHeadroomGb,
        model_dir: modelDir,
        cache_quota_gb: cacheQuotaGb,
        optimization_profile: optimizationProfile,
      }),
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

  knowledgeStatus: () =>
    fetch("/api/knowledge").then((r) => asJson<KnowledgeStatus>(r)),

  startKnowledgeIndex: () =>
    fetch("/api/knowledge/index", { method: "POST" }).then((r) =>
      asJson<{ started: boolean }>(r),
    ),

  cancelKnowledgeIndex: () =>
    fetch("/api/knowledge/index", { method: "DELETE" }).then((r) =>
      asJson<void>(r),
    ),

  searchKnowledge: (projectId: string, query: string, limit = 8) =>
    fetch("/api/knowledge/search", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ project_id: projectId, query, limit }),
    }).then((r) => asJson<{ hits: KnowledgeHit[] }>(r)),

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

  // Real `git log` for a project's directory, most recent 50 commits.
  // Empty list for a project that isn't a git checkout, not an error.
  gitLog: (projectId: string) =>
    fetch(`/api/projects/${encodeURIComponent(projectId)}/git/log`).then((r) =>
      asJson<{ commits: GitLogEntry[] }>(r),
    ),

  // Real `git add -A` + `git commit -m` against a project's directory.
  // Throws with the real git error (e.g. "nothing to commit").
  gitCommit: (projectId: string, message: string) =>
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

  listApprovals: (chatId?: string) =>
    fetch(`/api/approvals${chatId ? `?chat_id=${encodeURIComponent(chatId)}` : ""}`).then((r) =>
      asJson<{ approvals: PendingApproval[] }>(r),
    ),

  resolveApproval: (id: string, decision: ApprovalDecision) =>
    fetch(`/api/approvals/${encodeURIComponent(id)}/resolve`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ decision }),
    }).then((r) => asJson<void>(r)),

  listSkills: () => fetch("/api/skills").then((r) => asJson<{ skills: Skill[] }>(r)),

  createSkillFromMarkdown: (markdown: string) =>
    fetch("/api/skills", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ markdown }),
    }).then((r) => asJson<Skill>(r)),

  // `folderPath` must already contain a SKILL.md -- same in-app
  // folder-browser flow as adding a workspace (see FolderBrowserBody).
  createSkillFromFolder: (folderPath: string) =>
    fetch("/api/skills", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ folder_path: folderPath }),
    }).then((r) => asJson<Skill>(r)),

  setSkillEnabled: (name: string, on: boolean) =>
    fetch(`/api/skills/${encodeURIComponent(name)}/enabled`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ on }),
    }).then((r) => asJson<void>(r)),

  deleteSkill: (name: string) =>
    fetch(`/api/skills/${encodeURIComponent(name)}`, { method: "DELETE" }).then((r) => asJson<void>(r)),

  listMcpServers: () => fetch("/api/mcp/servers").then((r) => asJson<{ servers: McpServer[] }>(r)),

  addMcpServer: (server: { name: string; command: string; args: string[]; env: Record<string, string> }) =>
    fetch("/api/mcp/servers", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(server),
    }).then((r) => asJson<McpServer>(r)),

  setMcpServerEnabled: (name: string, enabled: boolean) =>
    fetch(`/api/mcp/servers/${encodeURIComponent(name)}`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ enabled }),
    }).then((r) => asJson<McpServer>(r)),

  deleteMcpServer: (name: string) =>
    fetch(`/api/mcp/servers/${encodeURIComponent(name)}`, { method: "DELETE" }).then((r) => asJson<void>(r)),

  refreshMcpServer: (name: string) =>
    fetch(`/api/mcp/servers/${encodeURIComponent(name)}/refresh`, { method: "POST" }).then((r) =>
      asJson<McpServer>(r),
    ),

  listMarketplaces: () =>
    fetch("/api/marketplaces").then((r) => asJson<{ marketplaces: Marketplace[] }>(r)),

  addMarketplace: (url: string) =>
    fetch("/api/marketplaces", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ url }),
    }).then((r) => asJson<Marketplace>(r)),

  removeMarketplace: (slug: string) =>
    fetch(`/api/marketplaces/${encodeURIComponent(slug)}`, { method: "DELETE" }).then((r) => asJson<void>(r)),

  refreshMarketplace: (slug: string) =>
    fetch(`/api/marketplaces/${encodeURIComponent(slug)}/refresh`, { method: "POST" }).then((r) =>
      asJson<Marketplace>(r),
    ),

  listMarketplacePlugins: (slug: string) =>
    fetch(`/api/marketplaces/${encodeURIComponent(slug)}/plugins`).then((r) =>
      asJson<{ plugins: MarketplacePlugin[] }>(r),
    ),

  installPlugin: (slug: string, name: string) =>
    fetch(`/api/marketplaces/${encodeURIComponent(slug)}/plugins/${encodeURIComponent(name)}/install`, {
      method: "POST",
    }).then((r) => asJson<PluginInstallResult>(r)),

  uninstallPlugin: (slug: string, name: string) =>
    fetch(`/api/marketplaces/${encodeURIComponent(slug)}/plugins/${encodeURIComponent(name)}`, {
      method: "DELETE",
    }).then((r) => asJson<PluginUninstallResult>(r)),

  listAgentPlugins: () => fetch("/api/agent-plugins").then((r) => asJson<{ plugins: AgentPlugin[] }>(r)),

  installAgentPlugin: (source: string) =>
    fetch("/api/agent-plugins", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ source }),
    }).then((r) => asJson<AgentPluginInstallResult>(r)),

  uninstallAgentPlugin: (name: string) =>
    fetch(`/api/agent-plugins/${encodeURIComponent(name)}`, { method: "DELETE" }).then((r) =>
      asJson<PluginUninstallResult>(r),
    ),

  listPluginCatalog: (
    params: { q?: string; offset?: number; limit?: number; installed?: "installed" | "uninstalled" } = {},
  ) => {
    const qs = new URLSearchParams();
    if (params.q) qs.set("q", params.q);
    if (params.offset) qs.set("offset", String(params.offset));
    if (params.limit) qs.set("limit", String(params.limit));
    if (params.installed) qs.set("installed", params.installed);
    const suffix = qs.toString();
    return fetch(`/api/plugins/catalog${suffix ? `?${suffix}` : ""}`).then((r) =>
      asJson<{ data: CatalogPlugin[]; total: number }>(r),
    );
  },

  installCatalogPlugin: (id: string) =>
    fetch(`/api/plugins/catalog/${encodeURIComponent(id)}/install`, { method: "POST" }).then((r) =>
      asJson<unknown>(r),
    ),

  uninstallCatalogPlugin: (id: string) =>
    fetch(`/api/plugins/catalog/${encodeURIComponent(id)}`, { method: "DELETE" }).then((r) =>
      asJson<unknown>(r),
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
  checkpoint?: {
    operation: string;
    entries: {
      path: string;
      kind: string;
      content?: number[];
      link_target?: string;
      unix_mode?: number;
    }[];
  } | null;
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
  permission?: PermissionMode,
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

// Plain, non-streaming completion -- no session/project headers, so this
// never enters the tool-calling loop (used by context compaction to
// summarize older messages, which needs one text answer, not tools).
export async function chatCompletion(model: string, messages: ChatMessage[]): Promise<string> {
  const res = await fetch("/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model, messages, stream: false }),
  });
  const body = await asJson<{ choices: { message: { content: string } }[] }>(res);
  return body.choices[0]?.message.content ?? "";
}
