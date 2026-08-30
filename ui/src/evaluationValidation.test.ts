import assert from "node:assert/strict";
import test from "node:test";
import {
  positiveInteger,
  unusedNumberedId,
  validateAssertionValue,
} from "./evaluationValidation.ts";

test("positiveInteger accepts bounded whole numbers", () => {
  assert.equal(positiveInteger("32", "Tokens", 64), 32);
});

test("positiveInteger rejects fractions and values above the bound", () => {
  assert.throws(() => positiveInteger("1.5", "Tokens"));
  assert.throws(() => positiveInteger("65", "Tokens", 64));
});

test("unusedNumberedId fills the first available gap", () => {
  assert.equal(unusedNumberedId(["case-1", "case-3"], "case"), "case-2");
});

test("contains and regex assertions cannot be blank", () => {
  assert.throws(() => validateAssertionValue("contains", "  "));
  assert.throws(() => validateAssertionValue("regex", ""));
  assert.doesNotThrow(() => validateAssertionValue("exact", ""));
});
