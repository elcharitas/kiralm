import { test } from "node:test";
import assert from "node:assert/strict";
import { formatBytesPair } from "./format.ts";

test("formatBytesPair: same natural unit for both numbers", () => {
  assert.equal(formatBytesPair(14 * 1024 ** 3, 16 * 1024 ** 3), "14.0/16.0 GB");
});

test("formatBytesPair: used would independently format smaller than total's unit", () => {
  // 900 MB used, 16 GB total -- naive independent formatting would
  // give "900 MB" and "16 GB"; both must render in GB here.
  const result = formatBytesPair(900 * 1024 ** 2, 16 * 1024 ** 3);
  assert.equal(result, "0.9/16.0 GB");
});

test("formatBytesPair: both under 1024 stay in bytes with no decimals", () => {
  assert.equal(formatBytesPair(200, 800), "200/800 B");
});

test("formatBytesPair: used is zero", () => {
  assert.equal(formatBytesPair(0, 16 * 1024 ** 3), "0.0/16.0 GB");
});
