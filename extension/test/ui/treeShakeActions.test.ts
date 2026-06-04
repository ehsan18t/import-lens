import assert from "node:assert/strict";
import test from "node:test";
import { treeShakeActionReason } from "../../src/ui/treeShakeActionReason.js";
import type { ImportResult } from "../../src/ipc/protocol.js";

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
  error: null,
  diagnostics: [],
  ...overrides,
});

test("treeShakeActionReason explains non tree-shakeable import results", () => {
  assert.match(treeShakeActionReason(result({ is_cjs: true })) ?? "", /CommonJS/u);
  assert.match(treeShakeActionReason(result({ side_effects: true })) ?? "", /side effects/u);
  assert.match(treeShakeActionReason(result({ truly_treeshakeable: false })) ?? "", /not tree-shakeable/u);
});

test("treeShakeActionReason ignores already tree-shakeable and errored imports", () => {
  assert.equal(treeShakeActionReason(result()), null);
  assert.equal(treeShakeActionReason(result({ error: "failed" })), null);
});
