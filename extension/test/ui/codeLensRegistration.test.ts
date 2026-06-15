import assert from "node:assert/strict";
import test from "node:test";
import { nextCodeLensRegistrationAction } from "../../src/ui/codeLensRegistrationPolicy.js";
import type { ImportLensConfig } from "../../src/config.js";

const config = (overrides: Partial<ImportLensConfig> = {}): ImportLensConfig => ({
  enabled: true,
  display: "standard",
  inlineRenderer: "native",
  compression: "brotli",
  debounceMs: 300,
  showWarnings: true,
  useCodeLens: true,
  enableDiskCache: true,
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
