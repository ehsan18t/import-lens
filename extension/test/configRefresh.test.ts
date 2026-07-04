import assert from "node:assert/strict";
import test from "node:test";
import {
  applyDaemonStateTransition,
  refreshVisibleImportLensDocuments,
} from "../src/configRefresh.js";
import type { ImportLensConfig } from "../src/config.js";

const config = (enabled: boolean): ImportLensConfig => ({
  enabled,
  display: "inlayHint",
  inlineRenderer: "native",
  compression: "brotli",
  debounceMs: 300,
  showWarnings: true,
  useCodeLens: false,
  enableDiskCache: true,
  cacheMaxSizeMB: 512,
  cacheMaxAgeDays: 30,
  enableRegistryHints: false,
  logLevel: "error",
  budgets: {},
});

const document = (languageId: string, scheme = "file") => ({
  languageId,
  uri: {
    scheme,
    toString: () => `${scheme}:///app/src/file.${languageId}`,
  },
});

const daemonTransitionActions = (calls: string[]) => ({
  setStatus: (state: "ready" | "unavailable") => calls.push(`status:${state}`),
  prewarmPackageJson: () => calls.push("prewarm"),
  refreshPackageJsonHints: () => calls.push("pkgHints"),
  refreshPackageJsonDecorations: () => calls.push("pkgDecorations"),
  reanalyzeDocuments: () => calls.push("reanalyze"),
});

test("daemon ready transition reanalyzes open documents", () => {
  const calls: string[] = [];
  applyDaemonStateTransition("ready", daemonTransitionActions(calls));

  assert.ok(
    calls.includes("reanalyze"),
    `ready transition must reanalyze documents; got ${calls.join(",")}`,
  );
});

test("daemon unavailable transition only updates status", () => {
  const calls: string[] = [];
  applyDaemonStateTransition("unavailable", daemonTransitionActions(calls));

  assert.deepEqual(calls, ["status:unavailable"]);
});

test("refreshVisibleImportLensDocuments uiOnly refresh does not schedule re-analysis", () => {
  const scheduled: string[] = [];
  let reapplyInsights = 0;

  refreshVisibleImportLensDocuments(
    [document("typescript"), document("javascriptreact")],
    config(true),
    {
      schedule: (doc) => scheduled.push(doc.uri.toString()),
      clear: () => undefined,
      refreshDecorations: () => undefined,
      refreshBudgetDiagnostics: () => undefined,
      refreshInlayHints: () => undefined,
      refreshCodeLens: () => undefined,
      refreshPackageJsonHints: () => undefined,
      reapplyInsights: () => {
        reapplyInsights += 1;
      },
    },
    "uiOnly",
  );

  assert.deepEqual(scheduled, []);
  assert.equal(reapplyInsights, 1);
});

test("refreshVisibleImportLensDocuments reanalyzes supported visible file documents when enabled", () => {
  const scheduled: string[] = [];
  const cleared: string[] = [];
  let decorationRefreshes = 0;
  let budgetDiagnosticRefreshes = 0;
  let hintRefreshes = 0;
  let codeLensRefreshes = 0;
  let packageJsonHintRefreshes = 0;

  refreshVisibleImportLensDocuments(
    [document("typescript"), document("markdown"), document("javascript", "untitled")],
    config(true),
    {
      schedule: (doc) => scheduled.push(doc.uri.toString()),
      clear: (uri) => cleared.push(uri.toString()),
      refreshDecorations: () => decorationRefreshes++,
      refreshBudgetDiagnostics: () => budgetDiagnosticRefreshes++,
      refreshInlayHints: () => hintRefreshes++,
      refreshCodeLens: () => codeLensRefreshes++,
      refreshPackageJsonHints: () => packageJsonHintRefreshes++,
    },
  );

  assert.deepEqual(scheduled, ["file:///app/src/file.typescript"]);
  assert.deepEqual(cleared, []);
  assert.equal(decorationRefreshes, 1);
  assert.equal(budgetDiagnosticRefreshes, 1);
  assert.equal(hintRefreshes, 1);
  assert.equal(codeLensRefreshes, 1);
  assert.equal(packageJsonHintRefreshes, 1);
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
      refreshBudgetDiagnostics: () => undefined,
      refreshInlayHints: () => undefined,
      refreshCodeLens: () => undefined,
      refreshPackageJsonHints: () => undefined,
    },
  );

  assert.deepEqual(scheduled, []);
  assert.deepEqual(cleared, [
    "file:///app/src/file.typescript",
    "file:///app/src/file.javascriptreact",
  ]);
});
