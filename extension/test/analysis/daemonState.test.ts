import assert from "node:assert/strict";
import test from "node:test";
import { importAnalysisStateFromDaemon } from "../../src/analysis/daemonState.js";
import type { ImportAnalysisItem } from "../../src/ipc/protocol.js";
import { detectedImport } from "../helpers/detectedImport.js";

const item = (overrides: Partial<ImportAnalysisItem> = {}): ImportAnalysisItem => ({
  detected: detectedImport({ specifier: "alpha" }),
  status: "loading",
  ...overrides,
});

const ignoreMissing = (): void => {};

// The daemon answers an import whose engine build has not run with `loading`, and pushes
// its size when the build lands. Mapping that to `unavailable` — which is what happened
// before, by falling through — renders no hint at all (`importHintParts` returns null for
// unavailable) and captions the import "Daemon unavailable" in the state. It has to stay a
// live, visibly-measuring import, because a size IS coming for it.
test("importAnalysisStateFromDaemon keeps a loading import loading", () => {
  const state = importAnalysisStateFromDaemon(item({ status: "loading" }), ignoreMissing);

  assert.equal(state.status, "loading");
  assert.equal(state.result, undefined);
  assert.equal(state.message, undefined, "a measuring import is not an error");
  assert.equal(state.detected.specifier, "alpha");
});

test("importAnalysisStateFromDaemon carries a ready result through", () => {
  const state = importAnalysisStateFromDaemon(
    item({
      status: "ready",
      result: {
        specifier: "alpha",
        raw_bytes: 10,
        minified_bytes: 8,
        gzip_bytes: 6,
        brotli_bytes: 5,
        zstd_bytes: 5,
        cache_hit: false,
        side_effects: false,
        truly_treeshakeable: true,
        is_cjs: false,
        confidence: "high",
        confidence_reasons: [],
        error: null,
        diagnostics: [],
      },
    }),
    ignoreMissing,
  );

  assert.equal(state.status, "ready");
  assert.equal(state.result?.brotli_bytes, 5);
});

test("importAnalysisStateFromDaemon reports a missing package with its message", () => {
  const reported: string[] = [];
  const state = importAnalysisStateFromDaemon(
    item({ status: "missing", message: "Package not installed" }),
    (specifier, reason) => reported.push(`${specifier}:${reason}`),
  );

  assert.equal(state.status, "missing");
  assert.equal(state.message, "Package not installed");
  assert.deepEqual(reported, ["alpha:Package not installed"]);
});

test("importAnalysisStateFromDaemon falls back to unavailable", () => {
  const state = importAnalysisStateFromDaemon(item({ status: "unavailable" }), ignoreMissing);

  assert.equal(state.status, "unavailable");
  assert.equal(state.message, "Daemon unavailable");
});
