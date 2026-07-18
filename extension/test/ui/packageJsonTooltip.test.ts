import assert from "node:assert/strict";
import test from "node:test";
import type { ImportLensConfig } from "../../src/config.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { copyImportDiagnosticsCommand } from "../../src/ui/diagnostics.js";
import {
  refreshPackageJsonRegistryHintCommand,
  refreshPackageJsonRegistryHintsCommand,
} from "../../src/ui/packageJsonRegistryCommands.js";
import {
  packageJsonDependencyTooltipMarkdown,
  packageJsonDependencyTooltipTrustedCommands,
  packageJsonSectionSummaryTooltipMarkdown,
  packageJsonSectionSummaryTooltipTrustedCommands,
} from "../../src/ui/packageJsonTooltip.js";
import {
  importResultSizeMarkdown,
  tooltipForResultMarkdown,
} from "../../src/ui/tooltipMarkdown.js";

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
  registryCacheMaxSizeMB: 32,
  enableRegistryHints: true,
  verboseRegistryLogging: false,
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

const commandArgs = (markdown: string, command: string): unknown[] => {
  const escaped = command.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
  const match = markdown.match(new RegExp(`command:${escaped}\\?([^)]*)`, "u"));

  assert.ok(match, `Expected markdown to include ${command}`);
  return JSON.parse(decodeURIComponent(match[1])) as unknown[];
};

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

test("packageJsonDependencyTooltipMarkdown includes fetched time and single-package refresh action", () => {
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
        fetchedAt: 1_000,
      },
    },
    config(),
    {
      packageJsonUri: "file:///workspace/package.json",
      formatFetchedAt: (timestamp) => `time:${timestamp}`,
    },
  );

  assert.match(markdown, /Registry info fetched: time:1000/u);
  assert.match(markdown, /\$\(sync\) Refresh npm registry info/u);
  assert.deepEqual(commandArgs(markdown, refreshPackageJsonRegistryHintCommand), [
    "file:///workspace/package.json",
    "react",
    "18.2.0",
  ]);
});

test("packageJsonDependencyTooltipTrustedCommands keeps refresh and diagnostics permissions narrow", () => {
  assert.deepEqual(
    packageJsonDependencyTooltipTrustedCommands(
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result({
          diagnostics: [{ stage: "resolve", message: "Missing peer.", details: [] }],
        }),
      },
      config(),
      { packageJsonUri: "file:///workspace/package.json" },
    ),
    [copyImportDiagnosticsCommand, refreshPackageJsonRegistryHintCommand],
  );
  assert.deepEqual(
    packageJsonDependencyTooltipTrustedCommands(
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result(),
      },
      config({ enableRegistryHints: false }),
      { packageJsonUri: "file:///workspace/package.json" },
    ),
    [],
  );
});

test("packageJsonSectionSummaryTooltipMarkdown uses oldest fetched time and summary refresh action", () => {
  const markdown = packageJsonSectionSummaryTooltipMarkdown(
    "2/2 measured · 8.2 kB br",
    [
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result(),
        registryHint: { latestVersion: "19.0.0", fetchedAt: 5_000 },
      },
      {
        name: "lodash-es",
        section: "dependencies",
        status: "ready",
        result: result(),
        registryHint: { latestVersion: "4.17.21", fetchedAt: 3_000 },
      },
    ],
    config(),
    {
      packageJsonUri: "file:///workspace/package.json",
      section: "dependencies",
      formatFetchedAt: (timestamp) => `time:${timestamp}`,
    },
  );

  assert.match(markdown, /All registry info fetched since: time:3000/u);
  assert.match(markdown, /\$\(sync\) Refresh all npm registry info/u);
  assert.deepEqual(commandArgs(markdown, refreshPackageJsonRegistryHintsCommand), [
    "file:///workspace/package.json",
    "dependencies",
  ]);
});

test("packageJsonSectionSummaryTooltipMarkdown reports missing fetched times", () => {
  const markdown = packageJsonSectionSummaryTooltipMarkdown(
    "2/2 measured · 8.2 kB br",
    [
      {
        name: "react",
        section: "dependencies",
        status: "ready",
        result: result(),
        registryHint: { latestVersion: "19.0.0", fetchedAt: 5_000 },
      },
      {
        name: "unfetched",
        section: "dependencies",
        status: "missing",
        registryHint: null,
      },
    ],
    config(),
    {
      packageJsonUri: "file:///workspace/package.json",
      section: "dependencies",
      formatFetchedAt: (timestamp) => `time:${timestamp}`,
    },
  );

  assert.match(markdown, /Some registry info has not been fetched yet/u);
});

test("packageJsonSectionSummaryTooltipTrustedCommands trusts only summary refresh", () => {
  assert.deepEqual(
    packageJsonSectionSummaryTooltipTrustedCommands(config(), {
      packageJsonUri: "file:///workspace/package.json",
    }),
    [refreshPackageJsonRegistryHintsCommand],
  );
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

test("importResultSizeMarkdown renders sectioned size rows", () => {
  assert.equal(
    importResultSizeMarkdown(result(), "brotli"),
    [
      "**Size**",
      "- Selected Brotli: **1.5 kB br**",
      "- Raw: 12.0 kB",
      "- Minified: 4.6 kB",
      "- Gzip: 2.1 kB",
      "- Brotli: 1.5 kB",
      "- Zstd: 1.7 kB",
    ].join("\n"),
    "an import that ships no assets renders no breakdown at all",
  );
});

test("importResultSizeMarkdown names the assets folded into the size", () => {
  // The bytes are already IN the number above; this says how it is composed, so a UI kit's cost
  // reads as part JavaScript and part stylesheet rather than one opaque figure (B2).
  //
  // So every kind here has to FIT INSIDE the totals in `result()`, on all five axes. An earlier
  // version of this fixture declared 4,700 B of brotli assets inside a 1,500 B brotli total, which
  // taught the exact opposite of the invariant the comment above states. The font barely compresses
  // (woff2 is brotli inside already) and the CSS compresses well, which is why their ratios differ.
  assert.equal(
    importResultSizeMarkdown(
      result({
        asset_breakdown: [
          {
            kind: "css",
            raw_bytes: 3000,
            minified_bytes: 1000,
            gzip_bytes: 400,
            brotli_bytes: 320,
            zstd_bytes: 350,
          },
          {
            kind: "font",
            raw_bytes: 300,
            minified_bytes: 300,
            gzip_bytes: 295,
            brotli_bytes: 290,
            zstd_bytes: 292,
          },
        ],
      }),
      "brotli",
    ),
    [
      "**Size**",
      "- Selected Brotli: **1.5 kB br**",
      "- Raw: 12.0 kB",
      "- Minified: 4.6 kB",
      "- Gzip: 2.1 kB",
      "- Brotli: 1.5 kB",
      "- Zstd: 1.7 kB",
      "",
      "**Included assets** (Brotli)",
      "- CSS: 320 B",
      "- Fonts: 290 B",
    ].join("\n"),
  );
});

test("tooltipForResultMarkdown renders size and analysis sections", () => {
  const markdown = tooltipForResultMarkdown(
    result({
      shared_bytes: 700,
      confidence_reasons: ["Detected side-effect import."],
      side_effects: true,
    }),
    config(),
    "server",
  );

  assert.match(markdown, /\*\*Size\*\*/u);
  assert.match(markdown, /- Selected Brotli: \*\*1\.5 kB br\*\*/u);
  assert.match(markdown, /\*\*Analysis\*\*/u);
  assert.match(markdown, /- Runtime: server/u);
  assert.match(markdown, /- Confidence: \*\*High\*\*/u);
  assert.match(markdown, /- Shared in file: 700 B/u);
  assert.match(markdown, /\*\*Confidence notes\*\*/u);
  assert.match(markdown, /- Detected side-effect import\./u);
});

test("tooltipForResultMarkdown renders conservative sizing without em dash", () => {
  const markdown = tooltipForResultMarkdown(
    result({ side_effects: true, truly_treeshakeable: false }),
    config(),
  );

  assert.match(
    markdown,
    /\$\(warning\) \*\*Conservative sizing:\*\* Size may include unused exports or side-effect modules\./u,
  );
  assert.doesNotMatch(markdown, /—/u);
});

test("tooltipForResultMarkdown renders compact diagnostics for an unmeasured result", () => {
  const markdown = tooltipForResultMarkdown(
    // **Unmeasured** (ADR-0006): no size, ever. The fixture used to carry an error AND all five
    // sizes — the fabricated shape, which is exactly what made the tooltip's `if (result.error)`
    // gate look like it worked. The tooltip asks for the size now, so the fixture must be honest
    // about not having one, or the test would be checking a state the daemon cannot produce.
    result({
      raw_bytes: null,
      minified_bytes: null,
      gzip_bytes: null,
      brotli_bytes: null,
      zstd_bytes: null,
      error: "failed to resolve package",
      unmeasured_stage: "resolve",
      confidence: "low",
      confidence_reasons: ["Resolver returned no entry."],
      diagnostics: [
        { stage: "resolve", message: "Cannot resolve react.", details: ["from app.ts"] },
      ],
    }),
    config(),
  );

  assert.match(markdown, /\*\*Diagnostics\*\*/u);
  assert.match(markdown, /Import Lens could not compute this import size\./u);
  assert.match(markdown, /- Error: failed to resolve package/u);
  assert.match(markdown, /- Confidence: \*\*Low\*\*/u);
  assert.match(markdown, /- Resolver returned no entry\./u);
  assert.match(markdown, /\$\(copy\) Copy diagnostics/u);
  assert.equal(commandArgs(markdown, copyImportDiagnosticsCommand).length, 1);
});

test("packageJsonDependencyTooltipMarkdown explains stale cached registry data", () => {
  const markdown = packageJsonDependencyTooltipMarkdown(
    {
      name: "react",
      section: "dependencies",
      status: "ready",
      installedVersion: "18.2.0",
      registryHint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      registryHintRefreshStatus: "stale",
      registryHintRefreshError: "temporary registry failure",
    },
    config({ enableRegistryHints: true }),
    { formatFetchedAt: () => "cached-time" },
  );

  assert.match(markdown, /\$\(warning\) Showing cached registry data/);
  assert.match(markdown, /Refresh error: temporary registry failure/);
});

test("tooltipForResultMarkdown keeps normal import hover free of package registry details", () => {
  const markdown = tooltipForResultMarkdown(result(), config());

  assert.doesNotMatch(markdown, /Installed version:/u);
  assert.doesNotMatch(markdown, /Latest version:/u);
  assert.doesNotMatch(markdown, /Version status:/u);
});
