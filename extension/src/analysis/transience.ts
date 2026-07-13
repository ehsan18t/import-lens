import type { FileSizeDocumentResponse, ImportDiagnostic, ImportResult } from "../ipc/protocol.js";

/**
 * The engine failure stages that describe THIS RUN of the daemon rather than the code being
 * measured: a build cancelled at its own deadline, one that unwound, one whose runtime went away.
 *
 * Mirrors `stage::is_transient` in `daemon/src/engine/mod.rs`, which is the source of truth; the
 * two are kept in step by a drift check (`scripts/test/engine-stage-coordination.test.mjs`).
 *
 * A result these produced is NOT an error. The pipeline substitutes a conservative static size, so
 * it carries `error: null` and a perfectly plausible byte count — which is precisely why every
 * store that has ever recorded one has recorded it happily. The stage is the only evidence there
 * is.
 */
export const transientEngineStages: readonly string[] = ["timeout", "panic", "engine_gone"];

const transientStages = new Set(transientEngineStages);

const hasTransientStage = (diagnostics: readonly ImportDiagnostic[] | undefined): boolean =>
  (diagnostics ?? []).some((diagnostic) => transientStages.has(diagnostic.stage));

/**
 * Whether an import result is a measurement of the package, and so may be written to a store that
 * OUTLIVES it — the persisted import-cost history (`globalState`), which has no TTL, no cache
 * generation, and one row per import identity, so a fabricated size does not merely go stale: it
 * replaces that import's real historical baseline for good, and every future "was 17 KB, now 58 B"
 * trend is computed against a number that never happened.
 *
 * The daemon refuses to cache the same result for the same reason (FR-026c), and its caches at
 * least expire. Ours do not.
 */
export const isDurableImportResult = (result: ImportResult | undefined): result is ImportResult =>
  result !== undefined && !result.error && !hasTransientStage(result.diagnostics);

/**
 * Whether a document's totals are a measurement of the file, and so may be written to the
 * persisted bundle-impact history.
 *
 * Three ways they are not, and only one of them is an error. `incomplete` says an import that
 * belongs in the totals was never measured — its own build had not landed (`loading`), or a
 * transient failure fabricated its size — so the number is a floor, real enough to SHOW beside the
 * diagnostics that say so (FR-024a) and worthless as a historical data point: the next run's
 * honest total would read as a regression against it. A transient stage in the response's own
 * diagnostics says the combined build itself degraded the same way.
 */
export const isDurableFileSize = (
  response: Pick<FileSizeDocumentResponse, "error" | "diagnostics" | "incomplete">,
): boolean =>
  !response.error && response.incomplete !== true && !hasTransientStage(response.diagnostics);
