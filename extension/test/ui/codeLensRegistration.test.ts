import assert from "node:assert/strict";
import test from "node:test";
import type { ImportLensConfig } from "../../src/config.js";
import { nextCodeLensRegistrationAction } from "../../src/ui/codeLensRegistrationPolicy.js";

const config = (overrides: Partial<ImportLensConfig> = {}): ImportLensConfig => ({
  enabled: true,
  display: "standard",
  inlineRenderer: "native",
  compression: "brotli",
  debounceMs: 300,
  showWarnings: true,
  useCodeLens: true,
  enableDiskCache: true,
  cacheMaxSizeMB: 512,
  cacheMaxAgeDays: 30,
  enableRegistryHints: false,
  logLevel: "error",
  budgets: {},
  ...overrides,
});

test("nextCodeLensRegistrationAction disposes registration when CodeLens mode is inactive", () => {
  assert.equal(
    nextCodeLensRegistrationAction(config({ display: "inlayHint", useCodeLens: false }), true),
    "dispose",
  );
  assert.equal(
    nextCodeLensRegistrationAction(config({ display: "inlayHint", useCodeLens: false }), false),
    "noop",
  );
});

test("nextCodeLensRegistrationAction registers only when CodeLens mode is active and unregistered", () => {
  assert.equal(nextCodeLensRegistrationAction(config(), false), "register");
  assert.equal(nextCodeLensRegistrationAction(config(), true), "noop");
});
