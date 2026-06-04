import assert from "node:assert/strict";
import test from "node:test";
import {
  applyImportAnalysisInsights,
  insightLabelSuffix,
} from "../../src/analysis/insights.js";
import {
  importCostHistoryItem,
  importCostHistoryKey,
  recordImportCostHistory,
  type ImportCostHistoryItem,
} from "../../src/analysis/history.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type { DetectedImport } from "../../src/imports/types.js";
import type { ImportResult } from "../../src/ipc/protocol.js";

class MemoryStore {
  readonly values = new Map<string, unknown>();

  get<T>(key: string, defaultValue: T): T {
    return (this.values.get(key) as T | undefined) ?? defaultValue;
  }

  async update(key: string, value: unknown): Promise<void> {
    this.values.set(key, value);
  }
}

const detected = (overrides: Partial<DetectedImport> = {}): DetectedImport => ({
  specifier: "lodash-es",
  packageName: "lodash-es",
  named: ["debounce"],
  importKind: "named",
  syntax: "static",
  runtime: "component",
  line: 4,
  quoteEnd: { line: 4, character: 32 },
  statementRange: {
    start: { line: 4, character: 0 },
    end: { line: 4, character: 36 },
  },
  ...overrides,
});

const result = (overrides: Partial<ImportResult> = {}): ImportResult => ({
  specifier: "lodash-es",
  raw_bytes: 10_000,
  minified_bytes: 4_000,
  gzip_bytes: 1_800,
  brotli_bytes: 1_500,
  zstd_bytes: 1_700,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  error: null,
  diagnostics: [],
  ...overrides,
});

const readyState = (
  detectedOverrides: Partial<DetectedImport> = {},
  resultOverrides: Partial<ImportResult> = {},
): ImportAnalysisState => ({
  detected: detected(detectedOverrides),
  status: "ready",
  result: result(resultOverrides),
});

test("applyImportAnalysisInsights adds working-tree import cost deltas", () => {
  const [state] = applyImportAnalysisInsights([readyState()], {
    changedLines: new Set([4]),
    importCostHistory: [],
  });

  assert.equal(insightLabelSuffix(state.insights), " · +1.5 kB br");
  assert.match(state.insights?.[0]?.tooltip ?? "", /Working-tree change/u);
});

test("applyImportAnalysisInsights explains shared dependency modules", () => {
  const states = applyImportAnalysisInsights(
    [
      readyState({}, {
        shared_bytes: 300,
        module_breakdown: [{ path: "/workspace/node_modules/lodash-es/debounce.js", bytes: 300 }],
      }),
      readyState({ specifier: "my-ui-lib", packageName: "my-ui-lib", named: [], importKind: "default" }, {
        specifier: "my-ui-lib",
        shared_bytes: 300,
        module_breakdown: [{ path: "/workspace/node_modules/lodash-es/debounce.js", bytes: 300 }],
      }),
    ],
    { importCostHistory: [] },
  );

  assert.match(states[0]?.insights?.[0]?.tooltip ?? "", /my-ui-lib/u);
  assert.match(states[0]?.insights?.[0]?.tooltip ?? "", /debounce\.js/u);
});

test("applyImportAnalysisInsights warns about barrel re-export boundaries", () => {
  const [state] = applyImportAnalysisInsights(
    [
      readyState(
        { importKind: "namespace", syntax: "star_reexport", named: [] },
        { truly_treeshakeable: false },
      ),
    ],
    { importCostHistory: [] },
  );

  assert.equal(insightLabelSuffix(state.insights), " · barrel");
  assert.match(state.insights?.[0]?.tooltip ?? "", /Barrel re-export/u);
});

test("applyImportAnalysisInsights adds import cost history trends", () => {
  const previous: ImportCostHistoryItem = {
    ...importCostHistoryItem(detected(), result({ brotli_bytes: 1200 }), 1_700_000),
    timestamp: 1_700_000,
  };
  const [state] = applyImportAnalysisInsights([readyState()], {
    importCostHistory: [previous],
  });

  assert.match(state.insights?.[0]?.tooltip ?? "", /was 1.2 kB br/u);
  assert.match(state.insights?.[0]?.tooltip ?? "", /\+300 B/u);
});

test("recordImportCostHistory skips unchanged consecutive import entries", async () => {
  const store = new MemoryStore();
  const first = importCostHistoryItem(detected(), result(), 1_700_000);
  const duplicate = importCostHistoryItem(detected(), result(), 1_800_000);

  await recordImportCostHistory(store, [first]);
  await recordImportCostHistory(store, [duplicate]);

  assert.deepEqual(store.get<ImportCostHistoryItem[]>(importCostHistoryKey, []), [first]);
});
