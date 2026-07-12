import assert from "node:assert/strict";
import test from "node:test";
import { mergeRefreshedResults } from "../../src/analysis/refreshMerge.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { detectedImport } from "../helpers/detectedImport.js";

const result = (overrides: Partial<ImportResult> = {}): ImportResult => ({
  specifier: "test-lib",
  raw_bytes: 10_000,
  minified_bytes: 4_000,
  gzip_bytes: 1_800,
  brotli_bytes: 1_500,
  zstd_bytes: 1_700,
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

const state = (
  specifier: string,
  overrides: Partial<ImportAnalysisState> = {},
): ImportAnalysisState => ({
  detected: detectedImport({ specifier }),
  status: "ready",
  result: result({ specifier }),
  ...overrides,
});

test("mergeRefreshedResults replaces matched results in place, preserving order", () => {
  const existing = [state("alpha"), state("beta"), state("gamma")];
  const refreshed = result({ specifier: "beta", brotli_bytes: 999 });

  const outcome = mergeRefreshedResults(existing, [refreshed]);

  assert.equal(outcome.changed, true);
  assert.deepEqual(
    outcome.next.map((entry) => entry.detected.specifier),
    ["alpha", "beta", "gamma"],
    "order must be preserved",
  );
  assert.equal(outcome.next[1]?.result?.brotli_bytes, 999);
  assert.equal(outcome.next[1]?.status, "ready");
  // Unmatched states pass through by reference — no gratuitous copies.
  assert.equal(outcome.next[0], existing[0]);
  assert.equal(outcome.next[2], existing[2]);
});

test("mergeRefreshedResults reports no change when nothing matches", () => {
  const existing = [state("alpha")];

  const outcome = mergeRefreshedResults(existing, [result({ specifier: "unrelated" })]);

  assert.equal(outcome.changed, false, "a miss must not trigger a store write or onDidChange");
  assert.deepEqual(outcome.next, existing);
});

test("mergeRefreshedResults ignores errored refreshed results", () => {
  const existing = [state("alpha", { result: result({ specifier: "alpha", brotli_bytes: 10 }) })];

  const outcome = mergeRefreshedResults(existing, [
    result({ specifier: "alpha", brotli_bytes: 999, error: "analysis failed" }),
  ]);

  assert.equal(outcome.changed, false, "errored refreshes must not replace a good stale value");
  assert.deepEqual(outcome.next, existing);
  assert.equal(existing[0]?.result?.brotli_bytes, 10);
});

test("mergeRefreshedResults promotes a loading state to ready", () => {
  const existing = [state("alpha", { status: "loading", result: undefined })];

  const outcome = mergeRefreshedResults(existing, [result({ specifier: "alpha" })]);

  assert.equal(outcome.changed, true);
  assert.equal(outcome.next[0]?.status, "ready");
  assert.equal(outcome.next[0]?.result?.specifier, "alpha");
});

// The mirror image of "ignores errored refreshed results", and the reason that rule is
// stated over the STATE rather than over the result. A `loading` import is one the daemon
// answered without a size and is building off the response path; if its build genuinely
// fails, the failure is the only answer it will ever get. Dropping it — the rule that
// protects a good stale value from a failed revalidation — would leave that import reading
// "Calculating..." for the rest of the session.
test("mergeRefreshedResults lets an errored result settle an import that has no size yet", () => {
  const existing = [state("alpha", { status: "loading", result: undefined })];

  const outcome = mergeRefreshedResults(existing, [
    result({ specifier: "alpha", error: "engine build panicked" }),
  ]);

  assert.equal(outcome.changed, true);
  assert.equal(outcome.next[0]?.status, "ready");
  assert.equal(outcome.next[0]?.result?.error, "engine build panicked");
});

test("mergeRefreshedResults drops insights computed against the stale result", () => {
  const existing = [
    state("alpha", { insights: [{ tooltip: "was 1.5 KB, budget commentary for the OLD value" }] }),
  ];

  const outcome = mergeRefreshedResults(existing, [
    result({ specifier: "alpha", brotli_bytes: 5_000 }),
  ]);

  assert.equal(outcome.changed, true);
  assert.equal(
    outcome.next[0]?.insights,
    undefined,
    "insights caption the replaced value; a refresh must clear them until re-analysis",
  );
});

test("mergeRefreshedResults gives same-specifier variants their own size (no cross-assignment)", () => {
  // Two imports of the SAME package that differ only by import kind / named exports:
  //   import React from "react"          (default, no named)   -> one bundle size
  //   import { useState } from "react"   (named, ["useState"]) -> a DIFFERENT size
  // They share a specifier, so specifier-alone keying collapses them and stamps one
  // variant's size onto both. The batch carries per-import identities to disambiguate.
  const existing: ImportAnalysisState[] = [
    {
      detected: detectedImport({ specifier: "react", importKind: "default", named: [] }),
      status: "ready",
      result: result({ specifier: "react", brotli_bytes: 1 }),
    },
    {
      detected: detectedImport({ specifier: "react", importKind: "named", named: ["useState"] }),
      status: "ready",
      result: result({ specifier: "react", brotli_bytes: 2 }),
    },
  ];

  const outcome = mergeRefreshedResults(
    existing,
    [
      result({ specifier: "react", brotli_bytes: 111 }),
      result({ specifier: "react", brotli_bytes: 222 }),
    ],
    {
      identities: [
        { specifier: "react", import_kind: "default", named: [] },
        { specifier: "react", import_kind: "named", named: ["useState"] },
      ],
    },
  );

  assert.equal(outcome.changed, true);
  // Each variant must receive ITS OWN refreshed size — never the sibling's.
  assert.equal(
    outcome.next[0]?.result?.brotli_bytes,
    111,
    "default import must keep the default variant's refreshed size",
  );
  assert.equal(
    outcome.next[1]?.result?.brotli_bytes,
    222,
    "named import must keep the named variant's refreshed size",
  );
});

test("mergeRefreshedResults matches variants regardless of named-set order", () => {
  const existing: ImportAnalysisState[] = [
    {
      detected: detectedImport({ specifier: "react", importKind: "named", named: ["b", "a"] }),
      status: "ready",
      result: result({ specifier: "react", brotli_bytes: 2 }),
    },
  ];

  const outcome = mergeRefreshedResults(
    existing,
    [result({ specifier: "react", brotli_bytes: 777 })],
    {
      // Same set, different order — the identity key must normalize order.
      identities: [{ specifier: "react", import_kind: "named", named: ["a", "b"] }],
    },
  );

  assert.equal(outcome.changed, true);
  assert.equal(outcome.next[0]?.result?.brotli_bytes, 777);
});

test("mergeRefreshedResults drops a superseded (stale) refresh batch", () => {
  const existing = [state("alpha", { result: result({ specifier: "alpha", brotli_bytes: 10 }) })];

  // A batch whose analysis generation is no longer current for the document (the
  // user edited after it was computed) must NOT overwrite the current states —
  // mirroring listener.ts's freshness.isCurrent gate on updateFileSize.
  const stale = mergeRefreshedResults(
    existing,
    [result({ specifier: "alpha", brotli_bytes: 999 })],
    { isCurrent: false },
  );

  assert.equal(stale.changed, false, "a superseded batch must not trigger a store write");
  assert.equal(existing[0]?.result?.brotli_bytes, 10, "the current states are left untouched");

  // A batch that is still current IS applied.
  const current = mergeRefreshedResults(
    existing,
    [result({ specifier: "alpha", brotli_bytes: 999 })],
    { isCurrent: true },
  );

  assert.equal(current.changed, true, "a current batch is applied");
  assert.equal(current.next[0]?.result?.brotli_bytes, 999);
});
