import assert from "node:assert/strict";
import test from "node:test";
import { PackageJsonRequestLifecycle } from "../../src/guidance/packageJsonRequestLifecycle.js";

test("a passive re-trigger with identical text is coalesced once the request is claimed", () => {
  const lifecycle = new PackageJsonRequestLifecycle();

  assert.equal(lifecycle.shouldSkipUnchanged("a", "text"), false);
  const requestId = lifecycle.begin("a", "text");

  assert.equal(lifecycle.shouldSkipUnchanged("a", "text"), true);
  assert.equal(lifecycle.shouldSkipUnchanged("a", "edited"), false);
  assert.equal(lifecycle.isCurrent("a", requestId), true);
});

test("a newer request for the same key supersedes the older in-flight request", () => {
  const lifecycle = new PackageJsonRequestLifecycle();

  const first = lifecycle.begin("a", "t1");
  const second = lifecycle.begin("a", "t2");

  assert.equal(lifecycle.isCurrent("a", first), false);
  assert.equal(lifecycle.isCurrent("a", second), true);
});

test("supersedeAll invalidates in-flight requests immediately", () => {
  const lifecycle = new PackageJsonRequestLifecycle();

  const requestId = lifecycle.begin("a", "text");
  assert.equal(lifecycle.isCurrent("a", requestId), true);

  lifecycle.supersedeAll();

  assert.equal(lifecycle.isCurrent("a", requestId), false);
  assert.equal(lifecycle.shouldSkipUnchanged("a", "text"), false);
});

test("fail forgets the optimistic content record so retry can proceed", () => {
  const lifecycle = new PackageJsonRequestLifecycle();

  lifecycle.begin("a", "text");
  assert.equal(lifecycle.shouldSkipUnchanged("a", "text"), true);

  lifecycle.fail("a");

  assert.equal(lifecycle.shouldSkipUnchanged("a", "text"), false);
});
