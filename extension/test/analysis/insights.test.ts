import assert from "node:assert/strict";
import test from "node:test";
import {
  type ImportCostHistoryItem,
  importCostHistoryItem,
  importCostHistoryKey,
  recordImportCostHistory,
} from "../../src/analysis/history.js";
import { applyImportAnalysisInsights, insightLabelSuffix } from "../../src/analysis/insights.js";
import { mergeRefreshedResults } from "../../src/analysis/refreshMerge.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type {
  DetectedImport,
  ImportResult,
  RefreshedImportIdentity,
} from "../../src/ipc/protocol.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

class MemoryStore {
  readonly values = new Map<string, unknown>();

  get<T>(key: string, defaultValue: T): T {
    return (this.values.get(key) as T | undefined) ?? defaultValue;
  }

  async update(key: string, value: unknown): Promise<void> {
    this.values.set(key, value);
  }
}

const detected = (overrides: Partial<DetectedImport> = {}): DetectedImport =>
  detectedImport({
    specifier: "lodash-es",
    packageName: "lodash-es",
    named: ["debounce"],
    importKind: "named",
    line: 4,
    quoteEnd: { line: 4, character: 32 },
    specifierRange: sourceRange(4, 8, 31),
    statementRange: sourceRange(4, 0, 36),
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
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error: null,
  diagnostics: [],
  ...overrides,
});

/**
 * The history row for an import the test asserts IS measured. `importCostHistoryItem` returns
 * `undefined` for one that is not — a row is five sizes, and there is no row without them — so a
 * test that wants a row says so here rather than defaulting the absence away.
 */
const measuredHistoryItem = (
  detectedImportValue: DetectedImport,
  resultValue: ImportResult,
  timestamp: number,
): ImportCostHistoryItem => {
  const item = importCostHistoryItem(detectedImportValue, resultValue, timestamp);
  assert.ok(item, "the fixture result must be measured");
  return item;
};

const readyState = (
  detectedOverrides: Partial<DetectedImport> = {},
  resultOverrides: Partial<ImportResult> = {},
): ImportAnalysisState => ({
  detected: detected(detectedOverrides),
  status: "ready",
  result: result(resultOverrides),
});

test("re-applying insights replaces rather than accumulates", () => {
  const base = readyState({}, { brotli_bytes: 50_000 });
  const options = { importCostHistory: [], budgets: { perImportBrotliBytes: 10_000 } };

  const once = applyImportAnalysisInsights([base], options);
  const twice = applyImportAnalysisInsights(once, options);

  const overBudget = (twice[0].insights ?? []).filter((insight) => insight.label === "over budget");
  assert.equal(overBudget.length, 1);
});

test("re-applying insights clears entries whose inputs no longer produce them", () => {
  const base = readyState({}, { brotli_bytes: 50_000 });
  const over = applyImportAnalysisInsights([base], {
    importCostHistory: [],
    budgets: { perImportBrotliBytes: 10_000 },
  });
  assert.ok((over[0].insights ?? []).some((insight) => insight.label === "over budget"));

  const relaxed = applyImportAnalysisInsights(over, { importCostHistory: [] });
  assert.equal((relaxed[0].insights ?? []).length, 0);
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
      readyState(
        {},
        {
          shared_bytes: 300,
          module_breakdown: [{ path: "/workspace/node_modules/lodash-es/debounce.js", bytes: 300 }],
        },
      ),
      readyState(
        { specifier: "my-ui-lib", packageName: "my-ui-lib", named: [], importKind: "default" },
        {
          specifier: "my-ui-lib",
          shared_bytes: 300,
          module_breakdown: [{ path: "/workspace/node_modules/lodash-es/debounce.js", bytes: 300 }],
        },
      ),
    ],
    { importCostHistory: [] },
  );

  assert.match(states[0]?.insights?.[0]?.tooltip ?? "", /my-ui-lib/u);
  assert.match(states[0]?.insights?.[0]?.tooltip ?? "", /debounce\.js/u);
});

// `import React, { useState } from "react"` is ONE specifier and TWO imports — a default and a
// named — and the daemon measures, and shares, by RESULT. Keyed by specifier, the two collapsed into
// one entry, no module had more than one "sharer", and the user was told the bytes the daemon had
// just reported as shared were "outside the public top-module breakdown". That is false: they are
// the sibling import of the same package, sitting on the same line.
const reactModule = { path: "/workspace/node_modules/react/index.js", bytes: 6_000 };

const reactDefaultDetected: Partial<DetectedImport> = {
  specifier: "react",
  packageName: "react",
  importKind: "default",
  named: [],
};

const reactNamedDetected: Partial<DetectedImport> = {
  specifier: "react",
  packageName: "react",
  importKind: "named",
  named: ["useState"],
};

const reactResult = (): ImportResult =>
  result({ specifier: "react", shared_bytes: 6_000, module_breakdown: [reactModule] });

test("applyImportAnalysisInsights names the sibling import that shares the same specifier", () => {
  const states = applyImportAnalysisInsights(
    [
      { detected: detected(reactDefaultDetected), status: "ready", result: reactResult() },
      { detected: detected(reactNamedDetected), status: "ready", result: reactResult() },
    ],
    { importCostHistory: [] },
  );

  const defaultTooltip = states[0]?.insights?.[0]?.tooltip ?? "";
  const namedTooltip = states[1]?.insights?.[0]?.tooltip ?? "";

  assert.match(defaultTooltip, /index\.js/u);
  assert.match(
    defaultTooltip,
    /react \{ useState \}/u,
    "the sharer is the OTHER result of the same specifier, and it must be named by its identity",
  );
  assert.doesNotMatch(
    defaultTooltip,
    /outside the public top-module breakdown/u,
    "the shared module is right there in the breakdown — claiming otherwise is the lie",
  );
  assert.match(namedTooltip, /react \(default\)/u);
});

test("the shared-dependency tooltip works on a COLD document, where results arrive by push", () => {
  const loading = (overrides: Partial<DetectedImport>): ImportAnalysisState => ({
    detected: detected(overrides),
    status: "loading",
  });
  const identity = (importKind: "default" | "named", named: string[]): RefreshedImportIdentity => ({
    specifier: "react",
    import_kind: importKind,
    named,
    runtime: "component",
  });

  // Nothing is measured yet: on a cold document every import lands as a push.
  const cold = [loading(reactDefaultDetected), loading(reactNamedDetected)];

  const merged = mergeRefreshedResults(cold, [reactResult(), reactResult()], {
    identities: [identity("default", []), identity("named", ["useState"])],
  });
  const states = applyImportAnalysisInsights(merged.next, { importCostHistory: [] });

  assert.equal(merged.changed, true);
  assert.match(
    states[0]?.insights?.[0]?.tooltip ?? "",
    /react \{ useState \}/u,
    "the pushed results carry shared_bytes and a breakdown; the insight must work off them too",
  );
});

// The message is TRUE whenever the shared module falls outside the top-10 breakdown the wire
// carries: the daemon knows the bytes are shared, and the extension genuinely cannot name what
// shares them. Only the specifier-collision case was a lie, and fixing that must not delete a
// message that is right the other half of the time.
test("applyImportAnalysisInsights still reports sharing outside the public top-module breakdown", () => {
  const states = applyImportAnalysisInsights(
    [
      readyState(
        { specifier: "alpha", packageName: "alpha" },
        {
          specifier: "alpha",
          shared_bytes: 300,
          module_breakdown: [{ path: "/workspace/node_modules/alpha/big.js", bytes: 9_000 }],
        },
      ),
      readyState(
        { specifier: "beta", packageName: "beta" },
        {
          specifier: "beta",
          shared_bytes: 300,
          module_breakdown: [{ path: "/workspace/node_modules/beta/big.js", bytes: 9_000 }],
        },
      ),
    ],
    { importCostHistory: [] },
  );

  assert.match(
    states[0]?.insights?.[0]?.tooltip ?? "",
    /outside the public top-module breakdown/u,
    "the 300 shared bytes are real and nothing in the top-10 breakdown accounts for them",
  );
});

// An Astro document: two frontmatter imports (server) both pull `shared.js`, and a client <script>
// import pulls it too. The two server imports genuinely share it — one Server chunk carries it once.
// The client import does NOT: a runtime is an artifact boundary, and the Client artifact ships its
// own copy (ADR-0005). Naming the client specifier as a sharer sells the user a deduplication the
// build model explicitly does not perform.
test("applyImportAnalysisInsights does not name a cross-runtime import as a sharer", () => {
  const sharedModule = { path: "/workspace/node_modules/shared-core/shared.js", bytes: 300 };

  const states = applyImportAnalysisInsights(
    [
      readyState(
        { specifier: "server-alpha", packageName: "server-alpha", runtime: "server" },
        { specifier: "server-alpha", shared_bytes: 300, module_breakdown: [sharedModule] },
      ),
      readyState(
        { specifier: "server-beta", packageName: "server-beta", runtime: "server" },
        { specifier: "server-beta", shared_bytes: 300, module_breakdown: [sharedModule] },
      ),
      readyState(
        { specifier: "client-gamma", packageName: "client-gamma", runtime: "client" },
        // The daemon now reports zero shared bytes for it, because within the Client runtime
        // nothing shares this module — that is the same fix, on the other side of the wire.
        { specifier: "client-gamma", shared_bytes: 0, module_breakdown: [sharedModule] },
      ),
    ],
    { importCostHistory: [] },
  );

  const tooltip = states[0]?.insights?.[0]?.tooltip ?? "";

  assert.match(tooltip, /server-beta/u, "sharing WITHIN the Server runtime is real and must show");
  assert.match(tooltip, /shared\.js/u);
  assert.doesNotMatch(
    tooltip,
    /client-gamma/u,
    "the client <script> import ships its own copy of the module — it saves the server imports \
     nothing, and naming it claims a deduplication that never happens (ADR-0005)",
  );

  assert.equal(
    states[2]?.insights?.length ?? 0,
    0,
    "and the cross-runtime import itself has no shared-dependency insight to show",
  );
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
  const previous = measuredHistoryItem(detected(), result({ brotli_bytes: 1200 }), 1_700_000);
  const [state] = applyImportAnalysisInsights([readyState()], {
    importCostHistory: [previous],
  });

  assert.match(state.insights?.[0]?.tooltip ?? "", /was 1.2 kB br/u);
  assert.match(state.insights?.[0]?.tooltip ?? "", /\+300 B/u);
});

test("recordImportCostHistory skips unchanged consecutive import entries", async () => {
  const store = new MemoryStore();
  const first = measuredHistoryItem(detected(), result(), 1_700_000);

  await recordImportCostHistory(store, [readyState()], 1_700_000);
  await recordImportCostHistory(store, [readyState()], 1_800_000);

  assert.deepEqual(store.get<ImportCostHistoryItem[]>(importCostHistoryKey, []), [first]);
});
