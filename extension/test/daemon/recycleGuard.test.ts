import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { RecycleGuard } from "../../src/daemon/recycleGuard.js";

test("recycle guard blocks more than five recycles in ten minutes", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-recycles-"));

  try {
    const guard = new RecycleGuard(root);
    const now = 1_800_000;

    await guard.recordRecycleTimes([now - 100, now - 90, now - 80, now - 70, now - 60, now - 50]);

    assert.equal(await guard.shouldEnterDegradedMode(now), true);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("recycle guard ignores older recycles outside the rolling window", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-recycles-"));

  try {
    const guard = new RecycleGuard(root);
    const now = 1_800_000;

    await guard.recordRecycleTimes([now - 700_000, now - 600_001, now - 90, now - 80, now - 70]);

    assert.equal(await guard.shouldEnterDegradedMode(now), false);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("recycle guard records clean daemon recycle events", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-recycles-"));

  try {
    const guard = new RecycleGuard(root);
    const now = 1_800_000;

    await guard.recordRecycle(now - 700_000);
    await guard.recordRecycle(now - 90);
    await guard.recordRecycle(now - 80);
    await guard.recordRecycle(now - 70);
    await guard.recordRecycle(now - 60);
    await guard.recordRecycle(now - 50);
    await guard.recordRecycle(now - 40);

    assert.equal(await guard.shouldEnterDegradedMode(now), true);
    assert.deepEqual(await guard.readRecycleTimes(), [now - 90, now - 80, now - 70, now - 60, now - 50, now - 40]);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("recycle guard preserves concurrent recycle records", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-recycles-"));

  try {
    const guard = new RecycleGuard(root);
    const now = 1_800_000;

    await Promise.all([
      guard.recordRecycle(now - 100),
      guard.recordRecycle(now - 90),
      guard.recordRecycle(now - 80),
      guard.recordRecycle(now - 70),
      guard.recordRecycle(now - 60),
      guard.recordRecycle(now - 50),
    ]);

    assert.deepEqual(await guard.readRecycleTimes(), [now - 100, now - 90, now - 80, now - 70, now - 60, now - 50]);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("recycle guard resets after a clean thirty minute session", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-recycles-"));

  try {
    const guard = new RecycleGuard(root);
    const now = 1_800_000;

    await guard.recordRecycleTimes([now - 1_900_000, now - 1_850_000]);
    await guard.resetAfterCleanSession(now);

    assert.deepEqual(await guard.readRecycleTimes(), []);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
