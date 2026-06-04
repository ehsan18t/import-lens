import assert from "node:assert/strict";
import test from "node:test";
import {
  shouldOfferImportCompletions,
  shouldShowDecorations,
  shouldShowInlayHints,
  shouldShowCodeLens,
} from "../../src/ui/displayGuards.js";
import type { ImportLensConfig } from "../../src/config.js";

const config = (overrides: Partial<ImportLensConfig> = {}): ImportLensConfig => ({
  enabled: true,
  display: "inlayHint",
  compression: "brotli",
  debounceMs: 300,
  showWarnings: true,
  useCodeLens: false,
  enableDiskCache: true,
  logLevel: "error",
  ...overrides,
});

test("display guards hide all ImportLens UI surfaces when disabled", () => {
  const disabled = config({ enabled: false, display: "standard", useCodeLens: true });

  assert.equal(shouldShowInlayHints(disabled), false);
  assert.equal(shouldShowCodeLens(disabled), false);
  assert.equal(shouldShowDecorations(disabled), false);
  assert.equal(shouldOfferImportCompletions(disabled), false);
});

test("display guards keep surfaces mutually consistent when enabled", () => {
  assert.equal(shouldShowInlayHints(config({ display: "inlayHint" })), true);
  assert.equal(shouldShowDecorations(config({ display: "standard", useCodeLens: false })), true);
  assert.equal(shouldShowCodeLens(config({ display: "standard", useCodeLens: true })), true);
  assert.equal(shouldOfferImportCompletions(config()), true);
});
