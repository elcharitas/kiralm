import assert from "node:assert/strict";
import test from "node:test";
import {
  COLLAPSED_ISLAND_WIDTH,
  MIN_MAIN_WIDTH,
  SHELL_HORIZONTAL_SPACE,
  fitExpandedIslands,
} from "./islandLayout.ts";

test("fitExpandedIslands preserves widths that fit", () => {
  assert.deepEqual(
    fitExpandedIslands(1040, 260, 340, false, false),
    [260, 340],
  );
});

test("fitExpandedIslands reclaims the required main width", () => {
  const container = 1040;
  const [left, right] = fitExpandedIslands(
    container,
    520,
    520,
    false,
    false,
  );
  assert.ok(left >= 190);
  assert.ok(right >= 190);
  assert.ok(
    left + right + SHELL_HORIZONTAL_SPACE + MIN_MAIN_WIDTH <= container,
  );
});

test("fitExpandedIslands budgets around a collapsed island", () => {
  const container = 760;
  const [, right] = fitExpandedIslands(
    container,
    520,
    520,
    true,
    false,
  );
  assert.ok(
    right + COLLAPSED_ISLAND_WIDTH + SHELL_HORIZONTAL_SPACE
      + MIN_MAIN_WIDTH <= container,
  );
});
