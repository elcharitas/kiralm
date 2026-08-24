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
  hot: boolean;
  last_used: string | null;
  verdict: string;
  verdict_reason: string;
  verdict_tier: VerdictTier;
}

export interface ApiError {
  error: { message: string; type?: string; param?: string; code?: string };
}

export interface CloudStatus {
  logged_in: boolean;
  device: string | null;
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
    fetch(`/api/models/${encodeURIComponent(id)}`).then((r) => asJson<ModelManifest>(r)),

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
};

export interface ChatMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

// Streams an OpenAI-shaped SSE response, calling onToken per delta.
export async function streamChat(
  model: string,
  messages: ChatMessage[],
  onToken: (text: string) => void,
  signal?: AbortSignal,
): Promise<void> {
  const res = await fetch("/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model, messages, stream: true }),
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
      } catch {
        // partial/malformed line, skip since the next chunk carries the rest
      }
    }
  }
}
