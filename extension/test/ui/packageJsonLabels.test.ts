import assert from "node:assert/strict";
import test from "node:test";
import type { ImportLensConfig } from "../../src/config.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import {
  packageJsonDependencyHintLabel,
  packageJsonDependencyHintParts,
  packageJsonDependencyVersionStatusLabel,
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
  cacheMaxSizeMB: 512,
  cacheMaxAgeDays: 30,
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

test("packageJsonDependencyHintLabel shows latest status for current dependencies", () => {
  assert.equal(
    packageJsonDependencyHintLabel(
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result(),
        registryHint: { latestVersion: "18.2.0", isLatest: true },
      },
      config(),
    ),
    "1.5 kB br · latest",
  );
});

test("packageJsonDependencyHintLabel shows update status for outdated dependencies", () => {
  assert.equal(
    packageJsonDependencyHintLabel(
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result(),
        registryHint: { latestVersion: "19.0.0", isLatest: false },
      },
      config(),
    ),
    "1.5 kB br · update 19.0.0",
  );
  assert.equal(
    packageJsonDependencyHintLabel(
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result({
          side_effects: true,
          truly_treeshakeable: false,
          is_cjs: true,
          confidence: "low",
        }),
        registryHint: { deprecated: true, latestVersion: "99.0.0", isLatest: false },
      },
      config({ display: "standard", compression: "gzip" }),
    ),
    "~2.1 kB gz · update 99.0.0",
  );
});

test("packageJsonDependencyHintLabel shows types only instead of zero-byte sizes", () => {
  assert.equal(
    packageJsonDependencyHintLabel(
      {
        name: "@types/node",
        section: "devDependencies",
        status: "ready",
        result: result({
          raw_bytes: 0,
          minified_bytes: 0,
          gzip_bytes: 0,
          brotli_bytes: 0,
          zstd_bytes: 0,
          diagnostics: [{ stage: "types_only", message: "Declaration-only package.", details: [] }],
        }),
        registryHint: { latestVersion: "22.15.3", isLatest: true },
      },
      config(),
    ),
    "types only · latest",
  );
});

test("packageJsonDependencyHintLabel omits sparkle from inline decorations", () => {
  assert.equal(
    packageJsonDependencyHintLabel(
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result(),
        registryHint: {
          latestVersion: "19.0.0",
          isLatest: false,
          latestPublishedAt: new Date(Date.now() - 60 * 60 * 1000).toISOString(),
        },
      },
      config(),
    ),
    "1.5 kB br · update 19.0.0",
  );
  assert.equal(
    packageJsonDependencyVersionStatusLabel({
      name: "react",
      section: "dependencies",
      status: "ready",
      result: result(),
      registryHint: {
        latestVersion: "19.0.0",
        isLatest: false,
        latestPublishedAt: new Date(Date.now() - 60 * 60 * 1000).toISOString(),
      },
    }),
    "✦ update 19.0.0",
  );
});

test("packageJsonDependencyHintParts assigns independent primary and suffix tones", () => {
  assert.deepEqual(
    packageJsonDependencyHintParts(
      {
        name: "typescript",
        section: "devDependencies",
        status: "unavailable",
        registryHint: { latestVersion: "5.9.3", isLatest: true },
      },
      config(),
    ),
    {
      primary: "unavailable",
      primaryTone: "unavailable",
      suffix: "latest",
      suffixTone: "latest",
    },
  );
});

test("packageJsonDependencyHintLabel formats unresolved states without daemon wording", () => {
  assert.equal(
    packageJsonDependencyHintLabel({ name: "react", section: "dependencies", status: "loading" }, config()),
    "checking...",
  );
  assert.equal(
    packageJsonDependencyHintLabel(
      {
        name: "missing",
        section: "dependencies",
        status: "missing",
        registryHint: { latestVersion: "1.2.3" },
      },
      config(),
    ),
    "not installed · install 1.2.3",
  );
  assert.equal(
    packageJsonDependencyHintLabel(
      {
        name: "react",
        section: "dependencies",
        status: "unavailable",
        registryHint: { latestVersion: "18.2.0", isLatest: true },
      },
      config(),
    ),
    "unavailable · latest",
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
