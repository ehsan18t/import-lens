import type { FileSizeDocumentResponse, ImportDiagnostic, ImportResult } from "../ipc/protocol.js";
import { measuredSizes } from "../ui/format.js";

/**
 * The engine failure stages that describe THIS RUN of the daemon rather than the code being
 * measured: a build cancelled at its own deadline, one that unwound, one whose runtime went away.
 *
 * Mirrors `stage::is_transient` in `daemon/src/engine/mod.rs`, which is the source of truth; the
 * two are kept in step by a drift check (`scripts/test/engine-stage-coordination.test.mjs`).
 *
 * Nothing substitutes a size any more — the fabricator is deleted, and a build these stages ended
 * produces no size at all, which is what makes the *first* shape below safe to detect by simply
 * asking whether there is a size. The stage still matters for the second shape: a result whose sizes
 * are REAL, and whose tree-shaking verdict was decided by a comparison build that timed out. There
 * the stage is the only evidence there is.
 */
export const transientEngineStages: readonly string[] = ["timeout", "panic", "engine_gone"];

const transientStages = new Set(transientEngineStages);

const hasTransientStage = (diagnostics: readonly ImportDiagnostic[] | undefined): boolean =>
  (diagnostics ?? []).some((diagnostic) => transientStages.has(diagnostic.stage));

const isTransientResult = (result: ImportResult): boolean =>
  // Two shapes. The result's own failure stage — the build that could not answer. And a transient
  // stage among the DIAGNOSTICS of an otherwise sound measurement: that is the full-package
  // comparison build having parked, which fabricates `truly_treeshakeable: false` on a real size.
  transientStages.has(result.unmeasured_stage ?? "") || hasTransientStage(result.diagnostics);

/**
 * Whether an import result may be written to a store that OUTLIVES it — the persisted import-cost
 * history (`globalState`), which has no TTL, no cache generation, and one row per import identity,
 * so a bad row does not merely go stale: it replaces that import's real historical baseline for
 * good, and every future "was 17 KB, now 58 B" trend is computed against a number that never
 * happened.
 *
 * Two refusals. A result with **no size** has nothing to record — the row IS five sizes. And a
 * result the daemon produced under a **transient** failure describes this moment's scheduling, not
 * the package; the daemon refuses to cache it for the same reason (FR-026c), and its caches at
 * least expire. Ours do not.
 */
export const isDurableImportResult = (result: ImportResult | undefined): result is ImportResult =>
  result !== undefined && measuredSizes(result) !== null && !isTransientResult(result);

/**
 * Whether a document's totals are a measurement of **this file**, and so may be written to a durable
 * store or judged against a budget.
 *
 * **This is the one predicate.** The daemon's `FileSizeComputation::is_cacheable` is its Rust twin,
 * and `cli/importlens.mjs` reads the same three fields off the same wire response — because the
 * defect this exists to end was three consumers each asking a slightly different question of the
 * same number, and the CLI asking the weakest one and issuing a CI verdict from it.
 *
 * Four ways the totals are not the file's, and only one of them is an error.
 *
 * - `error` — nothing was summed at all.
 * - `incomplete` — an import that belongs in the totals contributed no bytes: its own build had not
 *   landed (`loading`), or it could not be measured, for ANY reason (transient, deterministic, an
 *   entry that would not resolve, a package that is not installed). An UNDER-count.
 * - `degraded` — the file's own combined build failed, so the totals fell back to a sum of
 *   per-import costs with no shared-module deduplication. An OVER-count, and the one that carries no
 *   other signal: every contributor can be Measured, leaving `incomplete: false` and `error: null`.
 * - a transient stage among the response's own diagnostics — belt and braces with `degraded`, and
 *   the shape that catches a transient failure that reached the aggregate any other way.
 *
 * A total that fails this is still worth SHOWING (a floor beats a blank, FR-024a). It is never worth
 * writing down, and never worth a verdict (ADR-0006, invariant 5).
 */
export const isDurableFileSize = (
  response: Pick<FileSizeDocumentResponse, "error" | "diagnostics" | "incomplete" | "degraded">,
): boolean =>
  !response.error &&
  response.incomplete !== true &&
  response.degraded !== true &&
  !hasTransientStage(response.diagnostics);
