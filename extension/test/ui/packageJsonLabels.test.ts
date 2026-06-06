import assert from "node:assert/strict";
import test from "node:test";
import type { ImportLensConfig } from "../../src/config.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import {
  packageJsonDependencyHintLabel,
  packageJsonSectionSummaryLabel,
  type PackageJsonDependencyHintState,
} from "../../src/ui/packageJsonLabels.js";

const config = (overrides: Partial<ImportLensConfig> = {}): ImportLensConfig => ({
  enabled: true,
  display: "inlayHint",
  inlineRenderer: "native",
  compression: "brotli",
  debounceMs: 300,
  showWarnings: true,
  useCodeLens: false,
  enableDiskCache: true,
  enableRegistryHints: true,
  logLevel: "error",
  budgets: {},
  ...overrides,
});

const result = (overrides: Partial<ImportResult> = {}): ImportResult => ({
  specifier: "react",
  raw_bytes: 12000,
  minified_bytes: 4600,
  gzip_bytes: 2100,
  brotli_bytes: 1500,
  zstd_bytes: 1700,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: [],
  error: null,
  diagnostics: [],
  ...overrides,
});

test("packageJsonDependencyHintLabel formats compact measured dependency labels", () => {
  assert.equal(
    packageJsonDependencyHintLabel(
      { name: "react", section: "dependencies", status: "ready", result: result() },
      config(),
    ),
    "1.5 kB br",
  );
  assert.equal(
    packageJsonDependencyHintLabel(
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result(),
        registryHint: { deprecated: true, latestVersion: "99.0.0" },
      },
      config({ display: "standard", compression: "gzip" }),
    ),
    "2.1 kB gz · 4.6 kB min · deprecated",
  );
});

test("packageJsonDependencyHintLabel avoids noisy latest-version labels", () => {
  assert.equal(
    packageJsonDependencyHintLabel(
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result(),
        registryHint: { latestVersion: "99.0.0" },
      },
      config(),
    ),
    "1.5 kB br",
  );
});

test("packageJsonDependencyHintLabel formats unresolved states without daemon wording", () => {
  assert.equal(
    packageJsonDependencyHintLabel({ name: "react", section: "dependencies", status: "loading" }, config()),
    "checking...",
  );
  assert.equal(
    packageJsonDependencyHintLabel({ name: "missing", section: "dependencies", status: "missing" }, config()),
    "not installed",
  );
  assert.equal(
    packageJsonDependencyHintLabel({ name: "react", section: "dependencies", status: "unavailable" }, config()),
    "unavailable",
  );
});

test("packageJsonSectionSummaryLabel totals measured dependencies and problem counts", () => {
  const states: PackageJsonDependencyHintState[] = [
    { name: "react", section: "dependencies", status: "ready", result: result() },
    { name: "lodash-es", section: "dependencies", status: "ready", result: result({ brotli_bytes: 500 }) },
    { name: "missing", section: "dependencies", status: "missing" },
    { name: "vitest", section: "devDependencies", status: "ready", result: result({ brotli_bytes: 900 }) },
  ];

  assert.equal(
    packageJsonSectionSummaryLabel("dependencies", states, config()),
    "2/3 measured · 2.0 kB br · 1 not installed",
  );
});

test("packageJsonSectionSummaryLabel shows checking state before measurements arrive", () => {
  const states: PackageJsonDependencyHintState[] = [
    { name: "react", section: "dependencies", status: "loading" },
    { name: "lodash-es", section: "dependencies", status: "loading" },
  ];

  assert.equal(
    packageJsonSectionSummaryLabel("dependencies", states, config()),
    "2 checking...",
  );
});
