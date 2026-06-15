import assert from "node:assert/strict";
import test from "node:test";
import { applyStreamingBatchPartial } from "../../src/analysis/batchPartial.js";
import type { BatchResponse, ImportResult } from "../../src/ipc/protocol.js";

type TestState = {
  name: string;
  status: "loading" | "ready" | "missing";
  result?: ImportResult;
};

const resultFor = (specifier: string): ImportResult => ({
  specifier,
  raw_bytes: 100,
  minified_bytes: 50,
  gzip_bytes: 40,
  brotli_bytes: 30,
  zstd_bytes: 35,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: [],
  error: null,
  diagnostics: [],
});

test("applyStreamingBatchPartial merges partial frames into matching states", () => {
  const states: TestState[] = [
    { name: "lodash-es", status: "loading" },
    { name: "react", status: "loading" },
  ];
  let committed: readonly TestState[] = states;
  const partial: BatchResponse = {
    version: 2,
    request_id: 7,
    imports: [resultFor("react")],
    indexes: [1],
  };

  const next = applyStreamingBatchPartial(partial, {
    requestId: 7,
    isCurrent: (requestId) => requestId === 7,
    requestStateIndexes: [0, 1],
    states,
    isMissing: (state) => state.status === "missing",
    matchesResult: (state, result) => result.specifier === state.name,
    applyReady: (state, result) => ({ ...state, status: "ready" as const, result }),
    commit: (nextStates) => {
      committed = nextStates;
    },
  });

  assert.ok(next);
  assert.equal(committed[0]?.status, "loading");
  assert.equal(committed[1]?.status, "ready");
  assert.equal(committed[1]?.result?.specifier, "react");
});

test("applyStreamingBatchPartial ignores stale or mismatched partial frames", () => {
  const states: TestState[] = [{ name: "lodash-es", status: "loading" }];
  let commits = 0;

  const stale = applyStreamingBatchPartial(
    {
      version: 2,
      request_id: 9,
      imports: [resultFor("lodash-es")],
      indexes: [0],
    },
    {
      requestId: 8,
      isCurrent: (requestId) => requestId === 8,
      requestStateIndexes: [0],
      states,
      isMissing: () => false,
      matchesResult: (state, result) => result.specifier === state.name,
      applyReady: (state, result) => ({ ...state, status: "ready" as const, result }),
      commit: () => {
        commits += 1;
      },
    },
  );

  assert.equal(stale, null);
  assert.equal(commits, 0);
});
