import assert from "node:assert/strict";
import test from "node:test";
import type { ImportLensConfig } from "../../src/config.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { packageJsonDependencyTooltipMarkdown } from "../../src/ui/packageJsonTooltip.js";
import { tooltipForResultMarkdown } from "../../src/ui/tooltipMarkdown.js";

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

test("packageJsonDependencyTooltipMarkdown includes package registry freshness details", () => {
  const markdown = packageJsonDependencyTooltipMarkdown(
    {
      name: "react",
      section: "dependencies",
      status: "ready",
      installedVersion: "18.2.0",
      result: result(),
      registryHint: {
        latestVersion: "19.0.0",
        isLatest: false,
        latestPublishedAt: new Date(Date.now() - 60 * 60 * 1000).toISOString(),
      },
    },
    config(),
  );

  assert.match(markdown, /\*\*react\*\*/u);
  assert.match(markdown, /Installed version: 18\.2\.0/u);
  assert.match(markdown, /Latest version: 19\.0\.0/u);
  assert.match(markdown, /Version status: ✦ update 19\.0\.0/u);
  assert.match(markdown, /✦ New release under 24h/u);
  assert.match(markdown, /Latest published:/u);
});

test("packageJsonDependencyTooltipMarkdown includes type-only status", () => {
  const markdown = packageJsonDependencyTooltipMarkdown(
    {
      name: "@types/node",
      section: "devDependencies",
      status: "ready",
      installedVersion: "22.15.3",
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
  );

  assert.match(markdown, /Type-only package: yes/u);
  assert.match(markdown, /Version status: latest/u);
  assert.doesNotMatch(markdown, /0 B/u);
  assert.doesNotMatch(markdown, /0 B br · types only/u);
});

test("tooltipForResultMarkdown keeps normal import hover free of package registry details", () => {
  const markdown = tooltipForResultMarkdown(result(), config());

  assert.doesNotMatch(markdown, /Installed version:/u);
  assert.doesNotMatch(markdown, /Latest version:/u);
  assert.doesNotMatch(markdown, /Version status:/u);
});
