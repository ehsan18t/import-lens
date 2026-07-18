import type { FileSizeDocumentResponse } from "../ipc/protocol.js";
import { hasNonBudgetableDiagnostic } from "./budgetability.js";
import { hasTransientStage } from "./transience.js";

/**
 * **WHICH QUANTITY the daemon just handed over, and how sound it is.** The one model every surface
 * derives its words from.
 *
 * The daemon knows exactly what it is sending. The extension used to throw that away at the boundary
 * — `listener.ts` collapsed `incomplete` and `degraded` into a `~` character baked inside a `label`
 * string, and `StatusBarState` had no field for either — so **no surface downstream could tell the
 * truth even if it wanted to.** The status bar guessed, and guessed wrong, and the same defect was
 * then found one surface at a time: the editor budget, the report headline, the Shared Modules
 * table, the package.json summary, and the status bar.
 *
 * Two independent axes, because the daemon's two flags are independent (ADR-0006, invariant 4):
 *
 * - **`quantity`** — a `degraded` response is not a smaller or larger File Cost, it is a *different
 *   number*: the combined build failed, so what is on offer is the SUM of the per-import costs, and
 *   a module two imports reach is counted twice. ADR-0004 calls that a Combined Import Cost, an
 *   upper bound, and says it must never be presented as a size.
 * - **`short`** — bytes that belong in the number are missing (an import is still building,
 *   unmeasurable, or not installed; or supported asset bytes were not counted, deterministically or
 *   request-locally). A transient failure that *degrades* the aggregate is not short: it is the
 *   `combined-import-cost` quantity above, the combined build's own failure, and the `degraded` axis
 *   already reports it. Reading it as short too would claim a missing contributor that a timed-out
 *   combined build does not have.
 *
 * Both at once is a real state and neither word alone describes it: a fallback sum that is *also*
 * short double-counts some imports and omits others, so it is a bound in neither direction.
 */
export type FileCostQuantity = "file-cost" | "combined-import-cost";

export interface FileCostQuality {
  /** The quantity the number IS — not what the surface wishes it were. */
  quantity: FileCostQuantity;
  /** Some bytes that belong in the number contributed nothing to it. */
  short: boolean;
  /**
   * The number is a disclosed UPPER BOUND: every byte is present, but some are counted more than
   * once. Today's only producer is the per-sheet CSS fallback (`imprecise_assets`) — the stylesheet
   * set could not be bundled as one artifact, so each sheet was measured and compressed alone.
   *
   * A third axis rather than a flavour of `short`, because it points the OTHER way: `short` means
   * bytes are missing and the number is a floor. Folding an over-count into a floor would tell the
   * user the true cost is higher when it is lower.
   *
   * The extension had no axis for this at all. The daemon added a whole stage in order to SAY it,
   * the CLI printed the sentence, and the editor rendered a bare "File Cost" while its budget
   * silently returned `not-evaluated` — a number named as sound by one surface and refused by
   * another, on the same run.
   */
  imprecise: boolean;
}

/** The three fields on the wire that decide what the number is. Nothing here asks about `error`: a
 * response that failed outright has no number at all, and that is the one question `error` answers. */
export type FileCostFlags = Pick<
  FileSizeDocumentResponse,
  "diagnostics" | "incomplete" | "degraded"
>;

export const fileCostQuality = (response: FileCostFlags): FileCostQuality => ({
  quantity: response.degraded === true ? "combined-import-cost" : "file-cost",
  short:
    response.incomplete === true ||
    (response.degraded !== true && hasTransientStage(response.diagnostics)),
  imprecise: hasNonBudgetableDiagnostic(response.diagnostics),
});

/**
 * Whether the number is the file's size: one bundle over its imports, nothing missing.
 *
 * This is the same rule `isDurableFileSize` applies to decide whether the number may be stored or
 * judged, read for a different purpose — and the two are pinned together by a property test over the
 * whole flag space (`fileCostQuality.test.ts`), because a surface that NAMES a number the budget
 * refuses to JUDGE is exactly the contradiction this model exists to end.
 */
export const isFileCost = (quality: FileCostQuality): boolean =>
  quality.quantity === "file-cost" && !quality.short && !quality.imprecise;

/** What to call the number. A floor is not a File Cost, a per-import sum is not one either, and
 * neither is an over-count. */
export const fileCostQuantityName = (quality: FileCostQuality): string => {
  if (quality.quantity === "combined-import-cost") {
    return "Combined Import Cost";
  }

  if (quality.short) {
    return "File Cost floor";
  }

  return quality.imprecise ? "File Cost upper bound" : "File Cost";
};

/**
 * **The sentences, written ONCE.**
 *
 * `cli/importlens.mjs` mirrors these verbatim — it ships standalone and can import no TypeScript,
 * the same forced duplication as `transientStages` — and a drift check holds the two in lockstep
 * (`scripts/test/file-size-usability-coordination.test.mjs`). Two surfaces showing one number and
 * contradicting each other in words is how this got shipped: the CLI said the total was "an
 * un-deduplicated sum of its imports and not the file's size" while the status bar, on the same run,
 * called it a File Cost "built as one bundle".
 */
export const fileCostBecause = (quality: FileCostQuality): string => {
  const missingBytes =
    "bytes that belong in this file's total were not measured, so the number is a floor and not the file's size";
  const combinedBuildFailed =
    "the file's combined build failed, so the number is an un-deduplicated sum of its imports and not the file's size";
  const assetUpperBound =
    "asset processing produced a disclosed upper bound, so budgets were not evaluated";
  const builtAsOneBundle = "this file's imports built as one bundle";

  if (quality.quantity === "combined-import-cost") {
    return quality.short
      ? `${combinedBuildFailed}, and bytes that belong in it were not measured either`
      : combinedBuildFailed;
  }

  if (quality.short) {
    return missingBytes;
  }

  return quality.imprecise ? assetUpperBound : builtAsOneBundle;
};
