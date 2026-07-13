import assert from "node:assert/strict";
import test from "node:test";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type { DetectedImport, ImportResult } from "../../src/ipc/protocol.js";
import { shouldOfferNamedExportCandidates } from "../../src/ui/namedExportCandidatePolicy.js";
import { treeShakeActionReason } from "../../src/ui/treeShakeActionReason.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

const result = (overrides: Partial<ImportResult> = {}): ImportResult => ({
  specifier: "tiny-lib",
  raw_bytes: 100,
  minified_bytes: 80,
  gzip_bytes: 70,
  brotli_bytes: 60,
  zstd_bytes: 65,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error: null,
  diagnostics: [],
  ...overrides,
});

/**
 * **Unmeasured** (ADR-0006): no size, ever. The fixtures here used to say `{ error: "failed" }` and
 * keep all five sizes — the *fabricated* shape the daemon can no longer produce, and the one whose
 * existence made `!result.error` look like a working usability check. There is nothing to
 * tree-shake without a build, and now there is nothing to read, either.
 */
const unmeasured = (stage: string): ImportResult =>
  result({
    raw_bytes: null,
    minified_bytes: null,
    gzip_bytes: null,
    brotli_bytes: null,
    zstd_bytes: null,
    truly_treeshakeable: false,
    error: "engine build did not complete",
    unmeasured_stage: stage,
    diagnostics: [{ stage, message: "engine build did not complete", details: [] }],
  });

test("treeShakeActionReason explains non tree-shakeable import results", () => {
  assert.match(treeShakeActionReason(result({ is_cjs: true })) ?? "", /CommonJS/u);
  assert.match(treeShakeActionReason(result({ side_effects: true })) ?? "", /side effects/u);
  assert.match(
    treeShakeActionReason(result({ truly_treeshakeable: false })) ?? "",
    /not tree-shakeable/u,
  );
});

test("treeShakeActionReason ignores an already tree-shakeable import and one with no size", () => {
  assert.equal(treeShakeActionReason(result()), null);
  // Unmeasured, under a DETERMINISTIC stage and a TRANSIENT one. Neither has a build behind it, so
  // neither has a tree-shaking verdict to report — and `is_cjs`/`side_effects` on such a result are
  // the conservative defaults `ImportResult::unmeasured` stamps, not findings.
  assert.equal(treeShakeActionReason(unmeasured("parse")), null);
  assert.equal(treeShakeActionReason(unmeasured("timeout")), null);
});

const detected = (overrides: Partial<DetectedImport> = {}): DetectedImport =>
  detectedImport({
    specifier: "date-fns",
    packageName: "date-fns",
    quoteEnd: { line: 0, character: 31 },
    specifierRange: sourceRange(0, 8, 30),
    statementRange: sourceRange(0, 0, 33),
    ...overrides,
  });

const state = (
  detectedOverrides: Partial<DetectedImport> = {},
  resultOverrides: Partial<ImportResult> = {},
): ImportAnalysisState => ({
  detected: detected(detectedOverrides),
  status: "ready",
  result: result({ truly_treeshakeable: false, ...resultOverrides }),
});

test("shouldOfferNamedExportCandidates targets namespace imports that do not tree-shake", () => {
  assert.equal(shouldOfferNamedExportCandidates(state()), true);
  assert.equal(
    shouldOfferNamedExportCandidates(state({ importKind: "named", named: ["format"] })),
    false,
  );
  assert.equal(shouldOfferNamedExportCandidates(state({}, { truly_treeshakeable: true })), false);
  // No size, so no build, so no verdict to act on. Offering to narrow a namespace import of a
  // package nobody could measure is advice from nothing — and `!result.error` would have let a
  // still-LOADING import through, which has no error either.
  assert.equal(
    shouldOfferNamedExportCandidates({
      detected: detected(),
      status: "ready",
      result: unmeasured("parse"),
    }),
    false,
  );
});
