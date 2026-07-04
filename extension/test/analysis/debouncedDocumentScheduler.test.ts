import assert from "node:assert/strict";
import test from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import { DebouncedDocumentScheduler } from "../../src/analysis/debouncedDocumentScheduler.js";

test("DebouncedDocumentScheduler runs the callback once after the delay", async () => {
  const scheduler = new DebouncedDocumentScheduler();
  let runs = 0;

  scheduler.schedule("a", 5, () => runs++);
  assert.equal(runs, 0);

  await delay(25);
  assert.equal(runs, 1);
});

test("DebouncedDocumentScheduler collapses rapid reschedules of one key into a single run", async () => {
  const scheduler = new DebouncedDocumentScheduler();
  const runs: string[] = [];

  scheduler.schedule("a", 15, () => runs.push("first"));
  scheduler.schedule("a", 15, () => runs.push("second"));
  scheduler.schedule("a", 15, () => runs.push("third"));

  await delay(40);
  assert.deepEqual(runs, ["third"]);
});

test("DebouncedDocumentScheduler debounces each key independently", async () => {
  const scheduler = new DebouncedDocumentScheduler();
  const runs: string[] = [];

  scheduler.schedule("a", 5, () => runs.push("a"));
  scheduler.schedule("b", 5, () => runs.push("b"));

  await delay(25);
  assert.deepEqual(runs.sort(), ["a", "b"]);
});

test("DebouncedDocumentScheduler.cancel stops a pending run", async () => {
  const scheduler = new DebouncedDocumentScheduler();
  let runs = 0;

  scheduler.schedule("a", 15, () => runs++);
  scheduler.cancel("a");

  await delay(40);
  assert.equal(runs, 0);
});

test("DebouncedDocumentScheduler.dispose cancels every pending run", async () => {
  const scheduler = new DebouncedDocumentScheduler();
  let runs = 0;

  scheduler.schedule("a", 15, () => runs++);
  scheduler.schedule("b", 15, () => runs++);
  scheduler.dispose();

  await delay(40);
  assert.equal(runs, 0);
});

test("DebouncedDocumentScheduler forgets a key after it fires so the map does not retain dead timers", async () => {
  const scheduler = new DebouncedDocumentScheduler();
  const runs: string[] = [];

  scheduler.schedule("a", 5, () => runs.push("first"));
  await delay(25);

  scheduler.schedule("a", 5, () => runs.push("second"));
  await delay(25);

  assert.deepEqual(runs, ["first", "second"]);
});
