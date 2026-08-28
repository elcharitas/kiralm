import { Channel, invoke } from "@tauri-apps/api/core";

type ResponseHead = {
  status: number;
  statusText: string;
  headers: Record<string, string>;
};

type ResponseEvent =
  | { event: "chunk"; data: number[] }
  | { event: "end" }
  | { event: "error"; data: string };

function isDesktopApiRequest(input: RequestInfo | URL) {
  if (typeof input !== "string") return false;
  return input.startsWith("/api/") || input.startsWith("/v1/");
}

export function installDesktopTransport() {
  if (!("__TAURI_INTERNALS__" in window)) return;
  const browserFetch = window.fetch.bind(window);
  window.fetch = async (input: RequestInfo | URL, init: RequestInit = {}) => {
    if (!isDesktopApiRequest(input)) return browserFetch(input, init);

    let controller: ReadableStreamDefaultController<Uint8Array>;
    let closed = false;
    const body = new ReadableStream<Uint8Array>({
      start(next) {
        controller = next;
      },
      cancel() {
        closed = true;
      },
    });
    const onEvent = new Channel<ResponseEvent>();
    onEvent.onmessage = (message) => {
      if (closed) return;
      if (message.event === "chunk") controller.enqueue(Uint8Array.from(message.data));
      if (message.event === "end") {
        closed = true;
        controller.close();
      }
      if (message.event === "error") {
        closed = true;
        controller.error(new Error(message.data));
      }
    };
    init.signal?.addEventListener(
      "abort",
      () => {
        if (!closed) {
          closed = true;
          controller.error(new DOMException("The operation was aborted", "AbortError"));
        }
      },
      { once: true },
    );

    const headers = Object.fromEntries(new Headers(init.headers).entries());
    const head = await invoke<ResponseHead>("daemon_request", {
      request: {
        method: init.method ?? "GET",
        path: input,
        headers,
        body: typeof init.body === "string" ? init.body : null,
      },
      onEvent,
    });
    return new Response(head.status === 204 ? null : body, {
      status: head.status,
      statusText: head.statusText,
      headers: head.headers,
    });
  };
}
