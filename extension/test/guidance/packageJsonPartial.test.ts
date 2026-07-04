import assert from "node:assert/strict";
import test from "node:test";
import {
  markPackageJsonLoadingUnavailable,
  mergePackageJsonAnalysisPartial,
} from "../../src/guidance/packageJsonPartial.js";
import type {
  AnalyzePackageJsonResponse,
  ImportResult,
  PackageJsonDependencyAnalysisItem,
  PackageJsonDependencyEntry,
} from "../../src/ipc/protocol.js";

const entryFor = (name: string): PackageJsonDependencyEntry => ({
  name,
  version: "^1.0.0",
  section: "dependencies",
  range: {
    start: { line: 1, character: 2 },
    end: { line: 1, character: 10 },
  },
  nameRange: {
    start: { line: 1, character: 2 },
    end: { line: 1, character: 10 },
  },
  valueRange: {
    start: { line: 1, character: 12 },
    end: { line: 1, character: 20 },
  },
});

const resultFor = (specifier: string): ImportResult => ({
  specifier,
  raw_bytes: 100,
  minified_bytes: 80,
  gzip_bytes: 50,
  brotli_bytes: 40,
  zstd_bytes: 45,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: [],
  error: null,
  diagnostics: [],
});

const stateFor = (
  name: string,
  status: PackageJsonDependencyAnalysisItem["status"],
): PackageJsonDependencyAnalysisItem => ({
  entry: entryFor(name),
  name,
  section: "dependencies",
  status,
  installedVersion: "1.0.0",
});

test("mergePackageJsonAnalysisPartial preserves newer registry hints while applying indexed states", () => {
  const current: PackageJsonDependencyAnalysisItem[] = [
    {
      ...stateFor("react", "loading"),
      registryHint: {
        latestVersion: "19.0.0",
        isLatest: false,
        fetchedAt: 100,
      },
    },
  ];
  const partial: AnalyzePackageJsonResponse = {
    version: 5,
    request_id: 7,
    sections: [],
    indexes: [0],
    states: [
      {
        ...stateFor("react", "ready"),
        result: resultFor("react"),
      },
    ],
    error: null,
    diagnostics: [],
  };

  const merged = mergePackageJsonAnalysisPartial(current, partial);

  assert.equal(merged[0]?.status, "ready");
  assert.equal(merged[0]?.result?.specifier, "react");
  assert.equal(merged[0]?.registryHint?.latestVersion, "19.0.0");
});

test("mergePackageJsonAnalysisPartial ignores stale indexes and mismatched package names", () => {
  const current = [stateFor("react", "loading")];
  const partial: AnalyzePackageJsonResponse = {
    version: 5,
    request_id: 8,
    sections: [],
    indexes: [0],
    states: [stateFor("vue", "ready")],
    error: null,
    diagnostics: [],
  };

  assert.deepEqual(mergePackageJsonAnalysisPartial(current, partial), current);
});

test("mergePackageJsonAnalysisPartial preserves stale registry refresh status", () => {
  const current = [
    {
      ...stateFor("react", "ready"),
      registryHint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      registryHintRefreshStatus: "stale" as const,
      registryHintRefreshError: "temporary registry failure",
    },
  ];
  const partial: AnalyzePackageJsonResponse = {
    version: 7,
    request_id: 9,
    sections: [],
    states: [
      {
        ...stateFor("react", "ready"),
        result: resultFor("react"),
      },
    ],
    error: null,
    diagnostics: [],
  };

  const merged = mergePackageJsonAnalysisPartial(current, partial);

  assert.equal(merged[0]?.registryHintRefreshStatus, "stale");
  assert.equal(merged[0]?.registryHintRefreshError, "temporary registry failure");
});

test("markPackageJsonLoadingUnavailable preserves completed states and marks only loading states", () => {
  const ready = {
    ...stateFor("react", "ready"),
    result: resultFor("react"),
  };
  const loading = stateFor("vue", "loading");

  const next = markPackageJsonLoadingUnavailable([ready, loading], "Daemon unavailable");

  assert.equal(next[0], ready);
  assert.equal(next[1]?.status, "unavailable");
  assert.equal(next[1]?.message, "Daemon unavailable");
});
