import assert from "node:assert/strict";
import test from "node:test";
import { normalizePreviewUrl } from "./previewUrl.ts";

test("normalizePreviewUrl accepts only local HTTP previews", () => {
  assert.equal(normalizePreviewUrl("localhost:5173"), "http://localhost:5173/");
  assert.equal(normalizePreviewUrl("http://127.0.0.2:3000/app"), "http://127.0.0.2:3000/app");
  assert.equal(normalizePreviewUrl("http://[::1]:8080"), "http://[::1]:8080/");
  assert.equal(normalizePreviewUrl("https://example.com"), null);
  assert.equal(normalizePreviewUrl("file:///tmp/index.html"), null);
  assert.equal(normalizePreviewUrl("http://user:pass@localhost:3000"), null);
});
