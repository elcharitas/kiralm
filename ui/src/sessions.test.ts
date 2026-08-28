import { test } from "node:test";
import assert from "node:assert/strict";
import { titleFromMessage } from "./sessions.ts";

test("titleFromMessage: strips bold/italic/code/link syntax to plain text", () => {
  assert.equal(titleFromMessage("**Gamma** - **Delta**"), "Gamma - Delta");
  assert.equal(titleFromMessage("some `code` and a [link](https://example.com)"), "some code and a link");
});

test("titleFromMessage: strips list, heading, and blockquote markers", () => {
  assert.equal(titleFromMessage("- alpha\n- beta"), "alpha beta");
  assert.equal(titleFromMessage("## Heading\nbody"), "Heading body");
  assert.equal(titleFromMessage("> quoted line"), "quoted line");
});

test("titleFromMessage: falls back to New chat for empty input", () => {
  assert.equal(titleFromMessage(""), "New chat");
});

test("titleFromMessage: truncates long plain text at 48 chars with an ellipsis", () => {
  const long = "a".repeat(60);
  const result = titleFromMessage(long);
  assert.equal(result.length, 49);
  assert.ok(result.endsWith("…"));
});
