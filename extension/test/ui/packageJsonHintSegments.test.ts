import assert from "node:assert/strict";
import test from "node:test";
import type { ImportLensConfig } from "../../src/config.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import {
  packageJsonDependencyHintParts,
  type PackageJsonDependencyHintState,
} from "../../src/ui/packageJsonLabels.js";
import { packageJsonHintDisplayText, packageJsonHintSegments } from "../../src/ui/packageJsonHintSegments.js";

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

test("packageJsonHintSegments uses error primary and added-resource suffix for unavailable latest packages", () => {
  const parts = packageJsonDependencyHintParts(
    {
      name: "typescript",
      section: "devDependencies",
      status: "unavailable",
      registryHint: { latestVersion: "5.9.3", isLatest: true },
    } satisfies PackageJsonDependencyHintState,
    config(),
  );

  assert.deepEqual(packageJsonHintSegments(parts, config()), [
    {
      contentText: " unavailable",
      themeColorId: "list.errorForeground",
      fontStyle: "normal",
      margin: "0 0 0 0.75rem",
    },
    {
      contentText: " · latest",
      themeColorId: "gitDecoration.addedResourceForeground",
      fontStyle: "italic",
    },
  ]);
});

test("packageJsonHintSegments uses muted size and modified-resource update suffix", () => {
  const parts = packageJsonDependencyHintParts(
    {
      name: "react",
      section: "dependencies",
      status: "ready",
      result: result(),
      registryHint: { latestVersion: "19.0.0", isLatest: false },
    },
    config(),
  );

  assert.deepEqual(packageJsonHintSegments(parts, config()), [
    {
      contentText: " 1.5 kB br",
      themeColorId: "descriptionForeground",
      fontStyle: "normal",
      margin: "0 0 0 0.75rem",
    },
    {
      contentText: " · update 19.0.0",
      themeColorId: "gitDecoration.modifiedResourceForeground",
      fontStyle: "italic",
    },
  ]);
});

test("packageJsonHintDisplayText always renders primary before registry suffix", () => {
  const sized = packageJsonDependencyHintParts(
    {
      name: "oxc-parser",
      section: "dependencies",
      status: "ready",
      result: result({ brotli_bytes: 2100, confidence: "low" }),
      registryHint: { latestVersion: "0.136.0", isLatest: false },
    },
    config(),
  );
  const unavailable = packageJsonDependencyHintParts(
    {
      name: "typescript",
      section: "devDependencies",
      status: "unavailable",
      registryHint: { latestVersion: "5.9.3", isLatest: true },
    },
    config(),
  );

  assert.equal(packageJsonHintDisplayText(sized, config()), " ~2.1 kB br · update 0.136.0");
  assert.equal(packageJsonHintDisplayText(unavailable, config()), " unavailable · latest");
});

test("packageJsonHintSegments omits suffix when registry hints are disabled", () => {
  const parts = packageJsonDependencyHintParts(
    {
      name: "react",
      section: "dependencies",
      status: "ready",
      result: result(),
      registryHint: { latestVersion: "19.0.0", isLatest: false },
    },
    config({ enableRegistryHints: false }),
  );

  assert.equal(packageJsonHintSegments(parts, config({ enableRegistryHints: false })).length, 1);
});
