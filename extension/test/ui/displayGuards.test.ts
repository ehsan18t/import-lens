import assert from "node:assert/strict";
import test from "node:test";
import {
  shouldOfferImportCompletions,
  shouldShowColoredSourceHovers,
  shouldShowDecorations,
  shouldShowInlayHints,
  shouldShowNativeInlayHints,
  shouldShowCodeLens,
  shouldShowPackageJsonDecorations,
} from "../../src/ui/displayGuards.js";
import type { ImportLensConfig } from "../../src/config.js";

const config = (overrides: Partial<ImportLensConfig> = {}): ImportLensConfig => ({
  enabled: true,
  display: "inlayHint",
  inlineRenderer: "native",
  compression: "brotli",
  debounceMs: 300,
  showWarnings: true,
  useCodeLens: false,
  enableDiskCache: true,
  enableRegistryHints: false,
  logLevel: "error",
  budgets: {},
  ...overrides,
});

test("display guards hide all ImportLens UI surfaces when disabled", () => {
  const disabled = config({ enabled: false, display: "standard", useCodeLens: true });

  assert.equal(shouldShowInlayHints(disabled), false);
  assert.equal(shouldShowNativeInlayHints(disabled), false);
  assert.equal(shouldShowCodeLens(disabled), false);
  assert.equal(shouldShowDecorations(disabled), false);
  assert.equal(shouldOfferImportCompletions(disabled), false);
  assert.equal(shouldShowPackageJsonDecorations(disabled), false);
});

test("display guards keep surfaces mutually consistent when enabled", () => {
  assert.equal(shouldShowInlayHints(config()), true);
  assert.equal(shouldShowNativeInlayHints(config()), true);
  assert.equal(shouldShowDecorations(config()), false);
  assert.equal(shouldShowColoredSourceHovers(config()), false);
  assert.equal(shouldShowInlayHints(config({ display: "inlayHint", inlineRenderer: "colored" })), true);
  assert.equal(shouldShowDecorations(config({ display: "inlayHint", inlineRenderer: "colored" })), true);
  assert.equal(shouldShowColoredSourceHovers(config({ display: "inlayHint", inlineRenderer: "colored" })), true);
  assert.equal(shouldShowNativeInlayHints(config({ display: "inlayHint", inlineRenderer: "colored" })), false);
  assert.equal(shouldShowNativeInlayHints(config({ display: "inlayHint", inlineRenderer: "native" })), true);
  assert.equal(shouldShowDecorations(config({ display: "inlayHint", inlineRenderer: "native" })), false);
  assert.equal(shouldShowDecorations(config({ display: "standard", useCodeLens: false })), true);
  assert.equal(shouldShowCodeLens(config({ display: "standard", useCodeLens: true })), true);
  assert.equal(shouldShowCodeLens(config({ display: "inlayHint", useCodeLens: true })), false);
  assert.equal(shouldShowPackageJsonDecorations(config()), true);
  assert.equal(shouldOfferImportCompletions(config()), true);
});
