// Context compaction: once a chat's history gets long, summarize
// everything except the most recent messages into one system message
// instead of resending the full transcript every turn. Manual via the
// composer's "Compact chat" action or `/compact`; automatic when
// settings.auto_compact is on and the estimated size crosses the
// current model's context window (see shouldAutoCompact).
import { chatCompletion, type ChatMessage } from "./api.ts";
import type { StoredMessage } from "./sessions.ts";

// No real tokenizer available client-side -- chars/4 is the same
// order-of-magnitude estimate kiralm-web's own quota bookkeeping uses
// (app/lib/providers/workers-ai.ts's estimateTokens).
export function estimateTokens(text: string): number {
  return Math.max(1, Math.ceil(text.length / 4));
}

function estimateMessagesTokens(messages: { content: string }[]): number {
  return messages.reduce((sum, m) => sum + estimateTokens(m.content || ""), 0);
}

// Most recent messages kept verbatim; everything older gets folded into
// one summary. Small enough that a compact is still worth doing, large
// enough that the model doesn't lose the immediate thread of the turn.
const KEEP_RECENT = 6;
const COMPACT_PREFIX = "[Compacted summary of earlier conversation]\n";

// Trigger point as a fraction of the model's trained context window --
// leaves headroom for the summary itself, the next reply, and any tool
// output, rather than waiting until the window is nearly full.
const AUTO_COMPACT_THRESHOLD = 0.7;

export function shouldAutoCompact(messages: StoredMessage[], contextLength: number | null | undefined): boolean {
  if (!contextLength || messages.length <= KEEP_RECENT) return false;
  return estimateMessagesTokens(messages) > contextLength * AUTO_COMPACT_THRESHOLD;
}

export function isCompacted(messages: StoredMessage[]): boolean {
  return messages.length > 0 && messages[0].role === "system" && messages[0].content.startsWith(COMPACT_PREFIX);
}

// Real model call: asks `model` itself to summarize the older half of
// the transcript, then splices that summary in as a leading system
// message ahead of the kept-recent tail. Re-running on an
// already-compacted chat re-summarizes its existing summary alongside
// whatever's aged out of the recent window since, rather than
// stacking summaries.
export async function compactMessages(model: string, messages: StoredMessage[]): Promise<StoredMessage[]> {
  if (messages.length <= KEEP_RECENT) return messages;
  const older = messages.slice(0, -KEEP_RECENT);
  const recent = messages.slice(-KEEP_RECENT);
  const summary = await chatCompletion(model, [
    {
      role: "system",
      content:
        "Summarize the following conversation concisely, keeping any concrete facts, file paths, decisions, " +
        "and open tasks a continuation would need. Plain prose, no preamble.",
    },
    ...older.map(({ role, content }): ChatMessage => ({ role, content })),
  ]);
  return [{ role: "system", content: `${COMPACT_PREFIX}${summary}` }, ...recent];
}
