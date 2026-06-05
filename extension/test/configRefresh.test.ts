import assert from "node:assert/strict";
import test from "node:test";
import { refreshVisibleImportLensDocuments } from "../src/configRefresh.js";
import type { ImportLensConfig } from "../src/config.js";

const config = (enabled: boolean): ImportLensConfig => ({
  enabled,
  display: "inlayHint",
  inlineRenderer: "colored",
  compression: "brotli",
  debounceMs: 300,
  showWarnings: true,
  useCodeLens: false,
  enableDiskCache: true,
  logLevel: "error",
});

const document = (languageId: string, scheme = "file") => ({
  languageId,
  uri: {
    scheme,
    toString: () => `${scheme}:///app/src/file.${languageId}`,
  },
});

test("refreshVisibleImportLensDocuments reanalyzes supported visible file documents when enabled", () => {
  const scheduled: string[] = [];
  const cleared: string[] = [];
  let decorationRefreshes = 0;
  let hintRefreshes = 0;
  let codeLensRefreshes = 0;

  refreshVisibleImportLensDocuments(
    [document("typescript"), document("markdown"), document("javascript", "untitled")],
    config(true),
    {
      schedule: (doc) => scheduled.push(doc.uri.toString()),
      clear: (uri) => cleared.push(uri.toString()),
      refreshDecorations: () => decorationRefreshes++,
      refreshInlayHints: () => hintRefreshes++,
      refreshCodeLens: () => codeLensRefreshes++,
    },
  );

  assert.deepEqual(scheduled, ["file:///app/src/file.typescript"]);
  assert.deepEqual(cleared, []);
  assert.equal(decorationRefreshes, 1);
  assert.equal(hintRefreshes, 1);
  assert.equal(codeLensRefreshes, 1);
});

test("refreshVisibleImportLensDocuments clears supported visible file documents when disabled", () => {
  const scheduled: string[] = [];
  const cleared: string[] = [];

  refreshVisibleImportLensDocuments(
    [document("typescript"), document("javascriptreact"), document("markdown")],
    config(false),
    {
      schedule: (doc) => scheduled.push(doc.uri.toString()),
      clear: (uri) => cleared.push(uri.toString()),
      refreshDecorations: () => undefined,
      refreshInlayHints: () => undefined,
      refreshCodeLens: () => undefined,
    },
  );

  assert.deepEqual(scheduled, []);
  assert.deepEqual(cleared, [
    "file:///app/src/file.typescript",
    "file:///app/src/file.javascriptreact",
  ]);
});
