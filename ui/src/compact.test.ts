import { test } from "node:test";
import assert from "node:assert/strict";
import { estimateTokens, isCompacted, shouldAutoCompact } from "./compact.ts";
import type { StoredMessage } from "./sessions.ts";

function msg(content: string): StoredMessage {
  return { role: "user", content };
}

test("estimateTokens: rough chars/4 heuristic, minimum of 1", () => {
  assert.equal(estimateTokens(""), 1);
  assert.equal(estimateTokens("abcd"), 1);
  assert.equal(estimateTokens("abcdefgh"), 2);
});

test("shouldAutoCompact: false with no context length known", () => {
  const messages = Array.from({ length: 20 }, () => msg("x".repeat(1000)));
  assert.equal(shouldAutoCompact(messages, undefined), false);
  assert.equal(shouldAutoCompact(messages, null), false);
});

test("shouldAutoCompact: false at or under the keep-recent floor regardless of size", () => {
  const messages = Array.from({ length: 6 }, () => msg("x".repeat(100_000)));
  assert.equal(shouldAutoCompact(messages, 4096), false);
});

test("shouldAutoCompact: false comfortably under the threshold", () => {
  const messages = Array.from({ length: 10 }, () => msg("short"));
  assert.equal(shouldAutoCompact(messages, 4096), false);
});

test("shouldAutoCompact: true once estimated tokens cross 70% of context length", () => {
  // 4096 * 0.7 = ~2867 tokens needed; 10 messages of 1200 chars each
  // estimate to 300 tokens apiece, 3000 total.
  const messages = Array.from({ length: 10 }, () => msg("x".repeat(1200)));
  assert.equal(shouldAutoCompact(messages, 4096), true);
});

test("isCompacted: false for an empty or normal history", () => {
  assert.equal(isCompacted([]), false);
  assert.equal(isCompacted([msg("hi")]), false);
});

test("isCompacted: true once the leading message is a compaction summary", () => {
  const messages: StoredMessage[] = [
    { role: "system", content: "[Compacted summary of earlier conversation]\nWe discussed X." },
    msg("continuing"),
  ];
  assert.equal(isCompacted(messages), true);
});
