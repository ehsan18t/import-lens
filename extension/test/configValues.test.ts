import assert from "node:assert/strict";
import test from "node:test";
import { wholeMegabytes } from "../src/configValues.js";

test("a megabyte setting reaches the daemon as a whole number within the schema", () => {
  assert.equal(wholeMegabytes(100.5, 64, 512), 101);
  assert.equal(wholeMegabytes(-5, 64, 512), 64);
  assert.equal(wholeMegabytes(256, 64, 512), 256);
  assert.equal(wholeMegabytes(Number.POSITIVE_INFINITY, 64, 512), 512);
  assert.equal(wholeMegabytes("256", 64, 512), 512);
  assert.equal(wholeMegabytes(undefined, 1, 32), 32);
});
