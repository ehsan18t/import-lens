import assert from "node:assert/strict";
import test from "node:test";
import type { FileCostQuality } from "../../src/analysis/fileCostQuality.js";
import {
  type StatusBarState,
  statusBarText,
  statusBarTooltip,
} from "../../src/ui/statusbarText.js";

const sized = (
  bytes: number,
  quality: FileCostQuality,
  composition: readonly string[] = [],
): StatusBarState => ({
  kind: "size",
  bytes,
  compression: "brotli",
  quality,
  composition,
});

const fileCost: FileCostQuality = { quantity: "file-cost", short: false, imprecise: false };
const floor: FileCostQuality = { quantity: "file-cost", short: true, imprecise: false };
const combined: FileCostQuality = {
  quantity: "combined-import-cost",
  short: false,
  imprecise: false,
};
const upperBound: FileCostQuality = { quantity: "file-cost", short: false, imprecise: true };

test("statusBarText prefixes with IL and shows the size for a sized state", () => {
  assert.equal(statusBarText(sized(12300, fileCost)), "IL: 12.3 kB br");
});

test("statusBarText shows Ready when idle", () => {
  assert.equal(statusBarText({ kind: "ready" }), "IL: Ready");
});

test("statusBarText shows Computing while in flight", () => {
  assert.equal(statusBarText({ kind: "computing" }), "IL: Computing…");
});

test("statusBarText shows Unavailable on daemon/error", () => {
  assert.equal(statusBarText({ kind: "unavailable" }), "IL: Unavailable");
});

// The status bar shows the **File Cost**: ONE bundle over this file's imports, priced against an
// otherwise-empty app (ADR-0004). It is not a bundle size, and the product may not frame any figure
// as one — least of all the number the per-file budget is judged against.
test("statusBarTooltip names the File Cost and never calls it a bundle size", () => {
  assert.equal(
    statusBarTooltip(sized(12300, fileCost)),
    "Import Lens: File Cost (12.3 kB br) — this file's imports built as one bundle.",
  );
  assert.equal(statusBarTooltip({ kind: "ready" }), "Import Lens: Ready");
  assert.equal(statusBarTooltip({ kind: "computing" }), "Import Lens: Computing current file size");
  assert.equal(statusBarTooltip({ kind: "unavailable" }), "Import Lens: Daemon unavailable");
});

/**
 * **The fifth instance of the conflation, on the surface that is on screen all the time.**
 *
 * The file's combined build FAILED, so `brotli_bytes` holds a sum of the five imports' standalone
 * costs — 183.2 kB, against a real File Cost of 118.0 kB, on a file whose budget is 130 kB. Every
 * import is Measured, `incomplete` is false and `error` is null, so nothing else in the response
 * says a word about it.
 *
 * The status bar collapsed that into a `~` inside the label and told the reader it was a "File Cost
 * — this file's imports built as one bundle": the one thing that provably did not happen. It must
 * name the quantity it actually has, and it must say that no budget was judged from it — which is
 * what `importlens check` has said about the same number, on the same run, all along.
 */
test("a degraded total is a Combined Import Cost, and the status bar refuses to call it the file's size", () => {
  const state = sized(183_200, combined);

  assert.equal(statusBarText(state), "IL: ~183.2 kB br");
  assert.equal(
    statusBarTooltip(state),
    "Import Lens: Combined Import Cost (~183.2 kB br) — the file's combined build failed, so the \
number is an un-deduplicated sum of its imports and not the file's size. Budget not evaluated.",
  );
  assert.doesNotMatch(
    statusBarTooltip(state),
    /built as one bundle/u,
    "the combined build is exactly what failed - claiming it is the mechanism this number came from \
is a specific, confident, false claim about how it was produced",
  );
});

/** A floor is a lower bound. It is not the File Cost, and no verdict may be drawn from it. */
test("an incomplete total is a floor, and the status bar refuses to call it a File Cost", () => {
  const state = sized(118_000, floor);

  assert.equal(statusBarText(state), "IL: ~118.0 kB br");
  assert.equal(
    statusBarTooltip(state),
    "Import Lens: File Cost floor (~118.0 kB br) — bytes that belong in this file's total were not \
measured, so the number is a floor and not the file's size. Budget not evaluated.",
  );
});

/** Only a real File Cost gets a plain number and no "not evaluated" caveat. */
test("only a File Cost is shown without a mark, and only it can be judged", () => {
  assert.doesNotMatch(statusBarText(sized(118_000, fileCost)), /~/u);
  assert.doesNotMatch(statusBarTooltip(sized(118_000, fileCost)), /not evaluated/u);
  assert.match(statusBarTooltip(sized(118_000, floor)), /Budget not evaluated\./u);
  assert.match(statusBarTooltip(sized(183_200, combined)), /Budget not evaluated\./u);
});

/**
 * The fourth thing this surface can be handed, and until now the one it could not say.
 *
 * The stylesheets could not be bundled as one artifact, so each was measured and compressed alone
 * and the number reads HIGH. Every import is Measured, nothing is missing, and the combined build
 * succeeded — so `short` and `degraded` are both false and the status bar called it a plain "File
 * Cost", while the budget beside it silently declined to judge the same number.
 */
test("an imprecise total is named an upper bound, and no budget is judged from it", () => {
  const state = sized(120_400, upperBound);

  assert.equal(
    statusBarTooltip(state),
    "Import Lens: File Cost upper bound (~120.4 kB br) — asset processing produced a disclosed \
upper bound, so budgets were not evaluated. Budget not evaluated.",
  );
  assert.doesNotMatch(
    statusBarTooltip(state),
    /built as one bundle/u,
    "an over-count must not claim the clean-measurement explanation",
  );
});

/**
 * FR-018c requires every surface showing the size to be able to say what it is made of, and the
 * status bar is the only one on screen at all times. Carrying the quality alone leaves it unable to
 * mention that most of a total is stylesheet, while the import hover and the on-demand command both
 * say so — the number is correct and its explanation is not available anywhere the user is looking.
 */
test("statusBarTooltip says what a fused number is made of", () => {
  const tooltip = statusBarTooltip(sized(40000, fileCost, ["CSS 12.3 kB", "font 4.0 kB"]));
  assert.ok(tooltip.includes("Includes CSS 12.3 kB · font 4.0 kB."), tooltip);
});

test("statusBarTooltip stays silent when nothing but JavaScript is in the number", () => {
  assert.ok(!statusBarTooltip(sized(40000, fileCost)).includes("Includes"));
});
