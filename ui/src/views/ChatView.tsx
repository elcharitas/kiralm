import { useRef, useState } from "react";
import { streamChat, type ChatMessage, type StatusResponse } from "../api";

export function ChatView({ status }: { status: StatusResponse | null }) {
  const models = status?.models ?? [];
  const [model, setModel] = useState("");
  const [input, setInput] = useState("");
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);

  const effectiveModel = model || models[0]?.id || "";

  const send = async () => {
    const text = input.trim();
    if (!text || !effectiveModel || sending) return;
    setError(null);
    setInput("");
    const next = [...messages, { role: "user", content: text } as ChatMessage];
    setMessages([...next, { role: "assistant", content: "" }]);
    setSending(true);

    const controller = new AbortController();
    abortRef.current = controller;
    let assistantText = "";
    try {
      await streamChat(
        effectiveModel,
        next,
        (token) => {
          assistantText += token;
          setMessages([...next, { role: "assistant", content: assistantText }]);
        },
        controller.signal,
      );
    } catch (err) {
      if (!controller.signal.aborted) {
        setError(err instanceof Error ? err.message : String(err));
      }
    } finally {
      setSending(false);
    }
  };

  const stop = () => abortRef.current?.abort();

  return (
    <div className="view view-chat">
      <div className="view-header">
        <h1>Chat</h1>
        <select
          className="model-select mono"
          value={effectiveModel}
          onChange={(e) => setModel(e.target.value)}
        >
          {models.length === 0 && <option value="">no models downloaded</option>}
          {models.map((m) => (
            <option key={m.id} value={m.id}>
              {m.id} {m.hot ? "· hot" : ""}
            </option>
          ))}
        </select>
      </div>

      <div className="chat-log">
        {messages.length === 0 && (
          <div className="chat-empty">Send a message to test the local /v1/chat/completions API.</div>
        )}
        {messages.map((m, i) => (
          <div key={i} className={`chat-bubble chat-bubble-${m.role}`}>
            <div className="chat-role">{m.role}</div>
            <div className="chat-text">{m.content || (sending && i === messages.length - 1 ? "…" : "")}</div>
          </div>
        ))}
      </div>

      {error && <div className="banner banner-bad">{error}</div>}

      <form
        className="chat-input-row"
        onSubmit={(e) => {
          e.preventDefault();
          send();
        }}
      >
        <textarea
          className="chat-input"
          placeholder={effectiveModel ? `Message ${effectiveModel}…` : "Pull a model first"}
          value={input}
          disabled={!effectiveModel}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              send();
            }
          }}
        />
        {sending ? (
          <button type="button" className="btn btn-ghost" onClick={stop}>
            Stop
          </button>
        ) : (
          <button type="submit" className="btn btn-accent" disabled={!effectiveModel || !input.trim()}>
            Send
          </button>
        )}
      </form>
    </div>
  );
}
