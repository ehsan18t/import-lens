import assert from "node:assert/strict";
import test from "node:test";
import {
  markPackageJsonLoadingUnavailable,
  mergePackageJsonAnalysisPartial,
  packageJsonFinalResponseOutcome,
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

test("mergePackageJsonAnalysisPartial refines names-only package.json loading rows", () => {
  const namesOnly: AnalyzePackageJsonResponse = {
    version: 7,
    request_id: 10,
    sections: [],
    indexes: [0],
    states: [
      {
        ...stateFor("react", "loading"),
        installedVersion: undefined,
      },
    ],
    error: null,
    diagnostics: [],
  };
  const resolved: AnalyzePackageJsonResponse = {
    version: 7,
    request_id: 10,
    sections: [],
    indexes: [0],
    states: [
      {
        ...stateFor("react", "loading"),
        registryHint: {
          latestVersion: "19.0.0",
          isLatest: false,
          fetchedAt: 100,
        },
      },
    ],
    error: null,
    diagnostics: [],
  };
  const ready: AnalyzePackageJsonResponse = {
    version: 7,
    request_id: 10,
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

  const loading = mergePackageJsonAnalysisPartial([], namesOnly);
  const withVersion = mergePackageJsonAnalysisPartial(loading, resolved);
  const done = mergePackageJsonAnalysisPartial(withVersion, ready);

  assert.equal(loading[0]?.status, "loading");
  assert.equal(loading[0]?.installedVersion, undefined);
  assert.equal(withVersion[0]?.installedVersion, "1.0.0");
  assert.equal(done[0]?.status, "ready");
  assert.equal(done[0]?.result?.specifier, "react");
  assert.equal(done[0]?.registryHint?.latestVersion, "19.0.0");
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

test("mergePackageJsonAnalysisPartial keeps each dependency's own registry hint when rows shift", () => {
  const reactHint = { latestVersion: "19.0.0", isLatest: false, fetchedAt: 200 };
  const current = [
    {
      ...stateFor("react", "ready"),
      registryHint: reactHint,
      registryHintRefreshStatus: "stale" as const,
      registryHintRefreshError: "temporary registry failure",
    },
  ];
  const final: AnalyzePackageJsonResponse = {
    version: 8,
    request_id: 11,
    sections: [],
    states: [stateFor("zod", "ready"), stateFor("react", "ready")],
    error: null,
    diagnostics: [],
  };

  const merged = mergePackageJsonAnalysisPartial(current, final);

  assert.equal(merged[0]?.name, "zod");
  assert.equal(merged[0]?.registryHint, undefined);
  assert.equal(merged[0]?.registryHintRefreshStatus, undefined);
  assert.equal(merged[0]?.registryHintRefreshError, undefined);
  assert.equal(merged[1]?.registryHint, reactHint);
  assert.equal(merged[1]?.registryHintRefreshStatus, "stale");
});

test("mergePackageJsonAnalysisPartial drops a registry hint computed for another installed version", () => {
  const current = [
    {
      ...stateFor("react", "ready"),
      installedVersion: "18.0.0",
      registryHint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 200 },
    },
  ];
  const final: AnalyzePackageJsonResponse = {
    version: 8,
    request_id: 12,
    sections: [],
    states: [{ ...stateFor("react", "ready"), installedVersion: "19.0.0" }],
    error: null,
    diagnostics: [],
  };

  assert.equal(mergePackageJsonAnalysisPartial(current, final)[0]?.registryHint, undefined);
});

test("packageJsonFinalResponseOutcome keeps the last view while the manifest does not parse", () => {
  const empty = { states: [], error: null };
  const midEdit = '{\n  "dependencies": {\n    "react": "^19.0.0",\n  }\n}';

  assert.equal(packageJsonFinalResponseOutcome(empty, midEdit), "keep");
  assert.equal(packageJsonFinalResponseOutcome(empty, '{ "name": "app" }'), "clear");
  assert.equal(packageJsonFinalResponseOutcome({ states: [], error: "invalid" }, midEdit), "clear");
  assert.equal(
    packageJsonFinalResponseOutcome({ states: [stateFor("react", "ready")], error: null }, midEdit),
    "apply",
  );
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
