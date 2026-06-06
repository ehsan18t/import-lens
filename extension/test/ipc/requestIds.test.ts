import assert from "node:assert/strict";
import test from "node:test";
import { createIpcRequestIdGenerator } from "../../src/ipc/requestIds.js";

test("createIpcRequestIdGenerator returns strictly increasing ids", () => {
  const ids = [100, 100, 99, 150];
  const nextId = createIpcRequestIdGenerator(() => ids.shift() ?? 150);

  assert.deepEqual(
    [nextId(), nextId(), nextId(), nextId()],
    [101, 102, 150, 151],
  );
});
