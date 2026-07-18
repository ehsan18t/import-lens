import assert from "node:assert/strict";
import test from "node:test";
import {
  fileCostBecause,
  fileCostQuantityName,
  isFileCost,
  fileCostQuality as quality,
} from "../../src/analysis/fileCostQuality.js";
import { isDurableFileSize } from "../../src/analysis/transience.js";
import type { FileSizeDocumentResponse } from "../../src/ipc/protocol.js";

type Flags = Pick<FileSizeDocumentResponse, "diagnostics" | "incomplete" | "degraded">;

const flags = (overrides: Partial<Flags> = {}): Flags => ({
  diagnostics: [],
  incomplete: false,
  degraded: false,
  ...overrides,
});

const timeout = [{ stage: "timeout", message: "build cancelled", details: [] }];

/**
 * PROPERTY, over the whole flag space the daemon can send.
 *
 * `isDurableFileSize` decides whether the number may be STORED or judged; `fileCostQuality` decides
 * what the number may be CALLED. They are two readings of the same three fields, and the defect this
 * model exists to end is exactly what happens when they disagree: the budget refuses to evaluate a
 * `degraded` total while the status bar calls it a File Cost "built as one bundle".
 *
 * Quantified, not enumerated: add a fourth way a total can fail to be the file's, teach one of the
 * two about it and not the other, and this goes red.
 */
test("a total is durable if and only if it is a File Cost", () => {
  for (const incomplete of [true, false]) {
    for (const degraded of [true, false]) {
      for (const diagnostics of [[], timeout]) {
        const response = flags({ incomplete, degraded, diagnostics });

        assert.equal(
          isFileCost(quality(response)),
          isDurableFileSize({ ...response, error: null }),
          `the two readings of ${JSON.stringify(response)} disagree: one of the surfaces will name \
a number the other refuses to judge`,
        );
      }
    }
  }
});

test("a measured total is a File Cost, and says how it was built", () => {
  assert.deepEqual(quality(flags()), { quantity: "file-cost", short: false });
  assert.equal(fileCostQuantityName(quality(flags())), "File Cost");
  assert.equal(fileCostBecause(quality(flags())), "this file's imports built as one bundle");
});

/** A floor is not a File Cost, and must not be named one. */
test("an incomplete total is a floor and is never named a File Cost", () => {
  const floor = quality(flags({ incomplete: true }));

  assert.deepEqual(floor, { quantity: "file-cost", short: true });
  assert.equal(isFileCost(floor), false);
  assert.equal(fileCostQuantityName(floor), "File Cost floor");
  assert.equal(
    fileCostBecause(floor),
    "bytes that belong in this file's total were not measured, so the number is a floor and not the file's size",
  );
});

/**
 * The `degraded` shape, and the one this model exists for: every import Measured, `incomplete:
 * false`, `error: null` — and a number that is a SUM of per-import costs, counting a shared module
 * once per import. It is not the file's size, and it was being called one.
 */
test("a degraded total is a Combined Import Cost, never a File Cost built as one bundle", () => {
  const degraded = quality(flags({ degraded: true }));

  assert.deepEqual(degraded, { quantity: "combined-import-cost", short: false });
  assert.equal(isFileCost(degraded), false);
  assert.equal(fileCostQuantityName(degraded), "Combined Import Cost");
  assert.equal(
    fileCostBecause(degraded),
    "the file's combined build failed, so the number is an un-deduplicated sum of its imports and not the file's size",
  );
});

/**
 * Both at once — ADR-0006 invariant 4's third bullet. The fallback sum double-counts a shared module
 * AND is short an import's bytes, so it is a bound in neither direction, and neither word alone tells
 * the truth about it.
 */
test("a degraded total that is also short says both things", () => {
  const both = quality(flags({ degraded: true, incomplete: true }));

  assert.deepEqual(both, { quantity: "combined-import-cost", short: true });
  assert.equal(fileCostQuantityName(both), "Combined Import Cost");
  assert.equal(
    fileCostBecause(both),
    "the file's combined build failed, so the number is an un-deduplicated sum of its imports and not the file's size, and bytes that belong in it were not measured either",
  );
});

/**
 * The CANONICAL wire shape a combined-build TIMEOUT produces, and the one this fix exists for. The
 * daemon (`file_size.rs`, the `bundle_sync` Err arm) sets `degraded` AND pushes the timeout stage
 * into the aggregate's diagnostics, and leaves `incomplete` untouched — every contributor is still
 * Measured. The timeout is the combined build's OWN failure, which the `degraded` axis already
 * reports; it is NOT a missing contributor, so the number is a clean Combined Import Cost (an
 * over-count), not also short. The sentence must therefore NOT claim an import "was not measured
 * either", because every import WAS. This exact shape had no test — 6666efb rendered the degraded row
 * from a hand-built quality object with empty diagnostics — which is how the false clause shipped.
 */
test("a degraded total whose combined build timed out is a Combined Import Cost, not also short", () => {
  const timedOut = quality(flags({ degraded: true, diagnostics: timeout }));

  assert.deepEqual(timedOut, { quantity: "combined-import-cost", short: false });
  assert.equal(fileCostQuantityName(timedOut), "Combined Import Cost");
  assert.equal(
    fileCostBecause(timedOut),
    "the file's combined build failed, so the number is an un-deduplicated sum of its imports and not the file's size",
  );
});

/** A transient stage on the aggregate's own diagnostics leaves the number short, like any other gap. */
test("a transient stage on the aggregate makes the total a floor", () => {
  assert.deepEqual(quality(flags({ diagnostics: timeout })), {
    quantity: "file-cost",
    short: true,
  });
});
