import type { FileSizeDocumentResponse, ImportDiagnostic, ImportResult } from "../ipc/protocol.js";
import { measuredSizes } from "../ui/format.js";

/**
 * The analysis stages that describe THIS RUN of the daemon rather than the code being measured: an
 * engine that was lost, package bytes the filesystem could not expose, or a compressor that failed.
 *
 * Mirrors `TRANSIENT_ANALYSIS_STAGES` in `daemon/src/pipeline/stage.rs`, which is the source of
 * truth; the two are kept in step by a drift check
 * (`scripts/test/engine-stage-coordination.test.mjs`).
 *
 * Three wire shapes need the stage. A primary build can end with no size; a comparison build can
 * fail after the primary size was measured; and asset I/O/compression can leave a disclosed partial
 * size. The latter two still have numbers, so asking only whether a size exists is insufficient for
 * persistence and budget verdicts.
 */
export const transientAnalysisStages: readonly string[] = [
  "panic",
  "timeout",
  "engine_gone",
  "entry_metadata",
  "asset_io",
  "compression",
];

const transientStages = new Set(transientAnalysisStages);

/** Mirrors `pipeline::stage::DURABLE_RESULT_STAGES`. This is an allowlist on purpose: a future
 * diagnostic is refused from history and budgets until all three processes classify it. */
export const durableResultStages: readonly string[] = [
  "resolve",
  "parse",
  "link",
  "generate",
  "output_shape",
  "module_graph_limit",
  "missing_export",
  "ambiguous_export",
  "external",
  "uncounted_assets",
  "imprecise_assets",
  "package_validation",
  "package_resolution",
  "package_manifest",
  "entry_resolution",
  "oversized_entry",
  "minify",
  "types_only",
  "native_binary_only",
  "native_binary",
];

const durableStages = new Set(durableResultStages);

/** Exported for {@link ../analysis/fileCostQuality.fileCostQuality}, which asks the same question of
 * an aggregate's diagnostics in order to NAME the number rather than to store it. One definition of
 * "transient", read for two purposes. */
export const hasTransientStage = (diagnostics: readonly ImportDiagnostic[] | undefined): boolean =>
  (diagnostics ?? []).some((diagnostic) => transientStages.has(diagnostic.stage));

const hasOnlyDurableStages = (result: ImportResult): boolean =>
  (result.unmeasured_stage === null ||
    result.unmeasured_stage === undefined ||
    durableStages.has(result.unmeasured_stage)) &&
  result.diagnostics.every((diagnostic) => durableStages.has(diagnostic.stage));

/**
 * Whether an import result may be written to a store that OUTLIVES it — the persisted import-cost
 * history (`globalState`), which has no TTL, no cache generation, and one row per import identity,
 * so a bad row does not merely go stale: it replaces that import's real historical baseline for
 * good, and every future "was 17 KB, now 58 B" trend is computed against a number that never
 * happened.
 *
 * Two refusals. A result with **no size** has nothing to record — the row IS five sizes. And a
 * result produced under a **request-local** failure describes this machine/filesystem moment, not
 * the package; the daemon refuses to cache it for the same reason (FR-026c), and its caches at
 * least expire. Ours do not.
 */
export const isDurableImportResult = (result: ImportResult | undefined): result is ImportResult =>
  result !== undefined && measuredSizes(result) !== null && hasOnlyDurableStages(result);

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
 * - `incomplete` — bytes that belong in the totals are absent: an import's build had not landed or
 *   could not be measured, or a successful build disclosed supported `uncounted_assets`. An
 *   UNDER-count.
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
