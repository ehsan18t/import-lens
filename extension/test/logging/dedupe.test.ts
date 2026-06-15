import assert from "node:assert/strict";
import test from "node:test";
import { LogDedupe } from "../../src/logging/dedupe.js";

test("LogDedupe emits once per key", () => {
  const dedupe = new LogDedupe();
  const messages: string[] = [];

  dedupe.once("a", () => messages.push("first"));
  dedupe.once("a", () => messages.push("second"));
  dedupe.once("b", () => messages.push("other"));

  assert.deepEqual(messages, ["first", "other"]);
});

test("LogDedupe clear allows a key to emit again", () => {
  const dedupe = new LogDedupe();
  const messages: string[] = [];

  dedupe.once("a", () => messages.push("first"));
  dedupe.clear();
  dedupe.once("a", () => messages.push("second"));

  assert.deepEqual(messages, ["first", "second"]);
});
