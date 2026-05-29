# Component Script Imports Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make ImportLens detect and size imports in Svelte and Astro component scripts while preserving current JS/TS/React/Solid behavior and leaving room for Vue.

**Architecture:** Extend the extension host, not the daemon. VS Code activation and document selectors must include component file language IDs, and import parsing must first split component documents into JavaScript/TypeScript script regions, then feed those regions to OXC with absolute source offsets so inlay hints still land after the original import specifier. Astro regions must carry a runtime label because frontmatter code is server-side and stripped from browser output, while processed `<script>` tags can be bundled for the client.

**Tech Stack:** TypeScript 6.x, VS Code extension APIs, `oxc-parser`, Node test runner, framework-aware region extractors in the extension host, Rust daemon unchanged except for packaging hash refresh.

---

## Findings

1. `package.json` activates ImportLens only for `javascript`, `typescript`, `typescriptreact`, and `javascriptreact`; `svelte` files do not activate the extension.
2. `extension/src/config.ts` limits `supportedLanguageIds` and `languageSelector` to the same four language IDs, so even an already-active extension ignores Svelte documents in `DocumentAnalysisController.schedule()` and `analyze()`.
3. `extension/src/imports/parser.ts` sends the whole document buffer to `oxc-parser`. A full `.svelte` file is not valid JS/TS, so OXC recovery returns zero static imports for typical component markup.
4. The same whole-document parser problem applies to `.astro` files: Astro frontmatter and templates are not a pure JS/TS document.
5. Astro has two important script categories. Official Astro docs state that frontmatter code between `---` fences runs on the server and is stripped from the final page, while processed `<script>` tags can import local files or npm modules and are bundled as client-side module scripts.
6. SolidJS generally uses normal `.jsx`/`.tsx` files, so it should remain on the plain JS/TS/TSX path. It needs regression tests, not a separate parser.
7. The resolver and daemon are not the blocker. Once a component import is detected with the original component file path as `active_document_path`, existing package resolution and daemon sizing should work.

References:

- Astro component frontmatter is identified by `---` fences and is stripped from the browser page: <https://docs.astro.build/en/basics/astro-components/>
- Astro processed `<script>` tags support TypeScript and import bundling; scripts with extra attributes such as `is:inline` are not processed: <https://docs.astro.build/en/guides/client-side-scripts/>
- Astro supports npm package imports and TypeScript inside component scripts and hoisted scripts: <https://v4.docs.astro.build/en/guides/imports/>

## Effort Estimate

- Language activation and selectors for Svelte/Astro: **0.5 to 1 hour**
- Shared script region architecture and absolute position mapping: **2 to 3 hours**
- Svelte script region support: **1 to 1.5 hours**
- Astro frontmatter and processed client script region support: **2 to 3 hours**
- UI handling for Astro server-only imports: **1 to 1.5 hours**
- Tests for Svelte, Astro, Solid-style TSX, mixed runtime/type imports, and language support: **2 to 2.5 hours**
- SRS/README updates, Windows package rebuild, and verification: **1 to 1.5 hours**
- Total for Svelte + Astro + Solid regression coverage: **9.5 to 14 hours**

Optional follow-up support:

- Vue `<script>` / `<script setup>` using `@vue/compiler-sfc`: **3 to 4 additional hours**
- MDX import extraction: **3 to 5 additional hours** because imports can appear alongside Markdown/JSX and should be handled separately.

---

### Task 1: Add Language Support Constants

**Files:**
- Create: `extension/src/languages.ts`
- Modify: `extension/src/config.ts`
- Modify: `extension/src/listener.ts`
- Modify: `extension/src/extension.ts`
- Modify: `package.json`
- Test: `extension/test/languages.test.ts`

- [ ] **Step 1: Write the failing language support test**

Create `extension/test/languages.test.ts`:

```typescript
import assert from "node:assert/strict";
import test from "node:test";
import { supportedLanguageIds } from "../src/languages.js";

test("supportedLanguageIds includes Svelte and Astro component documents", () => {
  assert.equal(supportedLanguageIds.has("svelte"), true);
  assert.equal(supportedLanguageIds.has("astro"), true);
});
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```powershell
pnpm test:ts
```

Expected: TypeScript fails because `extension/src/languages.ts` does not exist.

- [ ] **Step 3: Create shared language constants**

Create `extension/src/languages.ts`:

```typescript
import type * as vscode from "vscode";

export const supportedLanguageIds: ReadonlySet<string> = new Set([
  "javascript",
  "typescript",
  "typescriptreact",
  "javascriptreact",
  "svelte",
  "astro",
]);

export const languageSelector: vscode.DocumentSelector = [
  { language: "javascript", scheme: "file" },
  { language: "typescript", scheme: "file" },
  { language: "typescriptreact", scheme: "file" },
  { language: "javascriptreact", scheme: "file" },
  { language: "svelte", scheme: "file" },
  { language: "astro", scheme: "file" },
];
```

- [ ] **Step 4: Move imports to the shared constants**

In `extension/src/config.ts`, remove `supportedLanguageIds` and `languageSelector`. Keep only configuration types and `getImportLensConfig()`.

In `extension/src/listener.ts`, replace:

```typescript
import { getImportLensConfig, supportedLanguageIds } from "./config.js";
```

with:

```typescript
import { getImportLensConfig } from "./config.js";
import { supportedLanguageIds } from "./languages.js";
```

In `extension/src/extension.ts`, replace:

```typescript
import { getImportLensConfig, languageSelector } from "./config.js";
```

with:

```typescript
import { getImportLensConfig } from "./config.js";
import { languageSelector } from "./languages.js";
```

- [ ] **Step 5: Add component activation events**

In `package.json`, add:

```json
"onLanguage:svelte",
"onLanguage:astro"
```

to `activationEvents` next to the existing language activation events.

- [ ] **Step 6: Verify language support passes**

Run:

```powershell
pnpm test:ts
pnpm check
```

Expected: all TypeScript tests pass and `tsc --noEmit` exits 0.

- [ ] **Step 7: Commit**

```powershell
git add package.json extension/src/languages.ts extension/src/config.ts extension/src/listener.ts extension/src/extension.ts extension/test/languages.test.ts
git commit -m "feat: register component documents for analysis" -m "Add Svelte and Astro to ImportLens activation events, document selectors, and supported language filtering so component files can enter the analysis pipeline."
```

---

### Task 2: Extract Component Script Regions

**Files:**
- Create: `extension/src/imports/scriptBlocks.ts`
- Modify: `extension/src/imports/parser.ts`
- Test: `extension/test/imports/scriptBlocks.test.ts`

- [ ] **Step 1: Write failing script region extraction tests**

Create `extension/test/imports/scriptBlocks.test.ts`:

```typescript
import assert from "node:assert/strict";
import test from "node:test";
import { scriptRegionsForDocument } from "../../src/imports/scriptBlocks.js";

test("scriptRegionsForDocument extracts Svelte TypeScript script content with absolute offset", () => {
  const source = [
    "<script lang=\"ts\">",
    "  import dayjs from 'dayjs';",
    "</script>",
    "<h1>{dayjs().format()}</h1>",
  ].join("\n");

  const blocks = scriptRegionsForDocument("Component.svelte", source);

  assert.equal(blocks.length, 1);
  assert.equal(blocks[0]?.language, "ts");
  assert.equal(blocks[0]?.runtime, "component");
  assert.equal(blocks[0]?.source.trim(), "import dayjs from 'dayjs';");
  assert.equal(source.slice(blocks[0]?.offset ?? -1).startsWith("\n  import dayjs"), true);
});

test("scriptRegionsForDocument extracts module and instance Svelte scripts", () => {
  const source = [
    "<script context=\"module\">",
    "  import { browser } from '$app/environment';",
    "</script>",
    "<script>",
    "  import dayjs from 'dayjs';",
    "</script>",
  ].join("\n");

  const blocks = scriptRegionsForDocument("Component.svelte", source);

  assert.deepEqual(blocks.map((block) => block.language), ["js", "js"]);
  assert.deepEqual(blocks.map((block) => block.runtime), ["component", "component"]);
  assert.equal(blocks.length, 2);
});

test("scriptRegionsForDocument extracts Astro frontmatter as server runtime", () => {
  const source = [
    "---",
    "import Icon from 'astro-icon';",
    "const title = 'Home';",
    "---",
    "<h1>{title}</h1>",
  ].join("\n");

  const blocks = scriptRegionsForDocument("Page.astro", source);

  assert.equal(blocks.length, 1);
  assert.equal(blocks[0]?.language, "ts");
  assert.equal(blocks[0]?.runtime, "server");
  assert.equal(blocks[0]?.source.includes("import Icon from 'astro-icon';"), true);
  assert.equal(blocks[0]?.offset, 4);
});

test("scriptRegionsForDocument extracts processed Astro client scripts", () => {
  const source = [
    "<h1>Demo</h1>",
    "<script>",
    "  import confetti from 'canvas-confetti';",
    "</script>",
    "<script is:inline>",
    "  import ignored from 'not-bundled';",
    "</script>",
  ].join("\n");

  const blocks = scriptRegionsForDocument("Page.astro", source);

  assert.equal(blocks.length, 1);
  assert.equal(blocks[0]?.runtime, "client");
  assert.equal(blocks[0]?.source.includes("canvas-confetti"), true);
});

test("scriptRegionsForDocument keeps plain JavaScript documents as a single block", () => {
  const source = "import dayjs from 'dayjs';";

  const blocks = scriptRegionsForDocument("sample.ts", source);

  assert.deepEqual(blocks, [{ filename: "sample.ts", source, offset: 0, language: "ts", runtime: "component" }]);
});
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```powershell
pnpm test:ts
```

Expected: TypeScript fails because `extension/src/imports/scriptBlocks.ts` does not exist.

- [ ] **Step 3: Implement script region extraction**

Create `extension/src/imports/scriptBlocks.ts`:

```typescript
import type { ParserOptions } from "oxc-parser";

export type ScriptRuntime = "component" | "server" | "client";

export interface ScriptRegion {
  filename: string;
  source: string;
  offset: number;
  language: ParserOptions["lang"];
  runtime: ScriptRuntime;
}

const svelteScriptPattern = /<script\b([^>]*)>([\s\S]*?)<\/script>/giu;
const astroClientScriptPattern = /<script\b([^>]*)>([\s\S]*?)<\/script>/giu;

const languageFromFilename = (filename: string): ParserOptions["lang"] => {
  if (filename.endsWith(".tsx")) {
    return "tsx";
  }

  if (filename.endsWith(".ts")) {
    return "ts";
  }

  if (filename.endsWith(".jsx")) {
    return "jsx";
  }

  return "js";
};

const languageFromAttributes = (attributes: string): ParserOptions["lang"] => {
  if (/\blang\s*=\s*["']ts["']/iu.test(attributes) || /\blang\s*=\s*ts\b/iu.test(attributes)) {
    return "ts";
  }

  return "js";
};

const blockFilename = (filename: string, language: ParserOptions["lang"], index: number): string =>
  `${filename}.${index}.${language}`;

const isProcessedAstroScript = (attributes: string): boolean => {
  const normalized = attributes.trim();
  return normalized === "" || /^src\s*=/iu.test(normalized);
};

const svelteRegions = (filename: string, source: string): ScriptRegion[] => {
  const regions: ScriptRegion[] = [];

  for (const match of source.matchAll(svelteScriptPattern)) {
    const fullMatch = match[0];
    const attributes = match[1] ?? "";
    const scriptSource = match[2] ?? "";
    const matchIndex = match.index ?? 0;
    const contentOffset = matchIndex + fullMatch.indexOf(">") + 1;
    const language = languageFromAttributes(attributes);

    regions.push({
      filename: blockFilename(filename, language, regions.length),
      source: scriptSource,
      offset: contentOffset,
      language,
      runtime: "component",
    });
  }

  return regions;
};

const astroRegions = (filename: string, source: string): ScriptRegion[] => {
  const regions: ScriptRegion[] = [];

  if (source.startsWith("---")) {
    const closingFence = source.indexOf("\n---", 3);

    if (closingFence !== -1) {
      regions.push({
        filename: blockFilename(filename, "ts", regions.length),
        source: source.slice(4, closingFence),
        offset: 4,
        language: "ts",
        runtime: "server",
      });
    }
  }

  for (const match of source.matchAll(astroClientScriptPattern)) {
    const fullMatch = match[0];
    const attributes = match[1] ?? "";
    const scriptSource = match[2] ?? "";
    const matchIndex = match.index ?? 0;
    const language = languageFromAttributes(attributes);

    if (!isProcessedAstroScript(attributes)) {
      continue;
    }

    const contentOffset = matchIndex + fullMatch.indexOf(">") + 1;

    regions.push({
      filename: blockFilename(filename, language, regions.length),
      source: scriptSource,
      offset: contentOffset,
      language,
      runtime: "client",
    });
  }

  return regions;
};

export const scriptRegionsForDocument = (filename: string, source: string): ScriptRegion[] => {
  if (filename.endsWith(".svelte")) {
    return svelteRegions(filename, source);
  }

  if (filename.endsWith(".astro")) {
    return astroRegions(filename, source);
  }

  return [{ filename, source, offset: 0, language: languageFromFilename(filename), runtime: "component" }];
};
```

- [ ] **Step 4: Verify script region tests pass**

Run:

```powershell
pnpm test:ts
```

Expected: script region tests pass; parser tests may still fail for Svelte/Astro imports until Task 3.

- [ ] **Step 5: Commit**

```powershell
git add extension/src/imports/scriptBlocks.ts extension/test/imports/scriptBlocks.test.ts
git commit -m "feat: extract component script regions" -m "Add a lightweight script region extractor for plain JS/TS files, Svelte scripts, Astro frontmatter, and processed Astro client scripts with absolute document offsets and runtime metadata."
```

---

### Task 3: Parse Script Regions With Absolute Positions

**Files:**
- Modify: `extension/src/imports/parser.ts`
- Test: `extension/test/imports/parser.test.ts`

- [ ] **Step 1: Write failing Svelte parser tests**

Add to `extension/test/imports/parser.test.ts`:

```typescript
test("extractRuntimeImports detects imports inside Svelte TypeScript script blocks", () => {
  const source = [
    "<script lang=\"ts\">",
    "  import dayjs, { type Dayjs } from 'dayjs';",
    "  import utc from 'dayjs/plugin/utc';",
    "</script>",
    "<h1>{dayjs().format()}</h1>",
  ].join("\n");

  const imports = extractRuntimeImports("Component.svelte", source);

  assert.deepEqual(
    imports.map((item) => ({
      specifier: item.specifier,
      kind: item.importKind,
      named: item.named,
      line: item.line,
      quoteLine: item.quoteEnd.line,
    })),
    [
      { specifier: "dayjs", kind: "default", named: [], line: 1, quoteLine: 1 },
      { specifier: "dayjs/plugin/utc", kind: "default", named: [], line: 2, quoteLine: 2 },
    ],
  );
});

test("extractRuntimeImports detects imports from both Svelte module and instance scripts", () => {
  const source = [
    "<script context=\"module\">",
    "  import { z } from 'zod';",
    "</script>",
    "<script>",
    "  import dayjs from 'dayjs';",
    "</script>",
  ].join("\n");

  const imports = extractRuntimeImports("Component.svelte", source);

  assert.deepEqual(
    imports.map((item) => ({ specifier: item.specifier, kind: item.importKind, line: item.line })),
    [
      { specifier: "zod", kind: "named", line: 1 },
      { specifier: "dayjs", kind: "default", line: 4 },
    ],
  );
});

test("extractRuntimeImports detects imports inside Astro frontmatter", () => {
  const source = [
    "---",
    "import Icon from 'astro-icon';",
    "import type { CollectionEntry } from 'astro:content';",
    "---",
    "<Icon name=\"home\" />",
  ].join("\n");

  const imports = extractRuntimeImports("Page.astro", source);

  assert.deepEqual(
    imports.map((item) => ({
      specifier: item.specifier,
      kind: item.importKind,
      runtime: item.runtime,
      line: item.line,
    })),
    [{ specifier: "astro-icon", kind: "default", runtime: "server", line: 1 }],
  );
});

test("extractRuntimeImports detects imports inside processed Astro client scripts", () => {
  const source = [
    "<h1>Demo</h1>",
    "<script>",
    "  import confetti from 'canvas-confetti';",
    "</script>",
    "<script is:inline>",
    "  import ignored from 'not-bundled';",
    "</script>",
  ].join("\n");

  const imports = extractRuntimeImports("Page.astro", source);

  assert.deepEqual(
    imports.map((item) => ({
      specifier: item.specifier,
      kind: item.importKind,
      runtime: item.runtime,
      line: item.line,
    })),
    [{ specifier: "canvas-confetti", kind: "default", runtime: "client", line: 2 }],
  );
});

test("extractRuntimeImports keeps Solid TSX on the plain parser path", () => {
  const source = [
    "import { createSignal } from 'solid-js';",
    "export const Counter = () => {",
    "  const [count, setCount] = createSignal(0);",
    "  return <button onClick={() => setCount(count() + 1)}>{count()}</button>;",
    "};",
  ].join("\n");

  const imports = extractRuntimeImports("Counter.tsx", source);

  assert.deepEqual(
    imports.map((item) => ({ specifier: item.specifier, kind: item.importKind, line: item.line })),
    [{ specifier: "solid-js", kind: "named", line: 0 }],
  );
});
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```powershell
pnpm test:ts
```

Expected: component parser tests fail because `extractRuntimeImports()` still parses the whole `.svelte` or `.astro` source and returns no imports for component files.

- [ ] **Step 3: Refactor parser to parse blocks**

In `extension/src/imports/parser.ts`:

1. Import `ScriptRegion` and `scriptRegionsForDocument`.
2. Remove the local `languageFromFilename` function if Task 2 already moved that responsibility to `scriptBlocks.ts`.
3. Change `createDetectedImport()` to accept an offset and use absolute offsets.
4. Add `runtime` to `DetectedImport` and set it from the script region.
5. Parse each script region separately and combine results.

Use this shape:

```typescript
import { scriptRegionsForDocument, type ScriptRegion } from "./scriptBlocks.js";
```

Change `createDetectedImport()`:

```typescript
const createDetectedImport = (
  documentSource: string,
  specifier: string,
  importKind: DetectedImport["importKind"],
  named: string[],
  start: number,
  end: number,
  quoteEndOffset: number,
  baseOffset: number,
  runtime: DetectedImport["runtime"],
): DetectedImport => ({
  specifier,
  packageName: getPackageName(specifier),
  named: [...named].sort(),
  importKind,
  line: positionAt(documentSource, baseOffset + start).line,
  quoteEnd: positionAt(documentSource, baseOffset + quoteEndOffset),
  statementRange: rangeFromOffsets(documentSource, baseOffset + start, baseOffset + end),
  runtime,
});
```

In `extension/src/imports/types.ts`, change `DetectedImport`:

```typescript
export type ImportRuntime = "component" | "server" | "client";

export interface DetectedImport {
  specifier: string;
  packageName: string;
  named: string[];
  importKind: ImportKind;
  line: number;
  quoteEnd: SourcePosition;
  statementRange: SourceRange;
  runtime: ImportRuntime;
}
```

Change parser helpers so they receive both `documentSource` and `region`, and call `createDetectedImport()` with `region.offset` and `region.runtime`.

Change `extractRuntimeImports()`:

```typescript
export const extractRuntimeImports = (filename: string, source: string): DetectedImport[] => {
  const imports: DetectedImport[] = [];

  for (const region of scriptRegionsForDocument(filename, source)) {
    imports.push(...extractRuntimeImportsFromRegion(source, region));
  }

  return imports.sort((left, right) => left.statementRange.start.line - right.statementRange.start.line);
};
```

Add `extractRuntimeImportsFromRegion()`:

```typescript
const extractRuntimeImportsFromRegion = (documentSource: string, region: ScriptRegion): DetectedImport[] => {
  const parsed = parseSync(region.filename, region.source, {
    ...parserOptions,
    lang: region.language,
  });
  const imports: DetectedImport[] = [];

  for (const item of parsed.module.staticImports) {
    imports.push(...importsFromStaticImport(documentSource, region, item));
  }

  for (const item of parsed.module.staticExports) {
    for (const entry of item.entries) {
      const detected = importFromStaticExport(documentSource, region, entry, item.start, item.end);

      if (detected) {
        imports.push(detected);
      }
    }
  }

  for (const item of parsed.module.dynamicImports) {
    const specifier = trimLiteralQuotes(region.source.slice(item.moduleRequest.start, item.moduleRequest.end));

    if (specifier && isRuntimePackageSpecifier(specifier)) {
      imports.push(createDetectedImport(documentSource, specifier, "dynamic", [], item.start, item.end, item.moduleRequest.end, region.offset, region.runtime));
    }
  }

  return imports;
};
```

- [ ] **Step 4: Verify parser tests pass**

Run:

```powershell
pnpm test:ts
```

Expected: all parser tests pass, including Svelte script imports, Astro frontmatter imports, Astro processed client script imports, and Solid TSX imports with absolute line positions.

- [ ] **Step 5: Commit**

```powershell
git add extension/src/imports/parser.ts extension/src/imports/types.ts extension/test/imports/parser.test.ts
git commit -m "feat: parse imports from component script regions" -m "Parse Svelte and Astro script regions with OXC, retain runtime metadata, and map import ranges back to absolute document positions so inlay hints and decorations render at the original import specifier."
```

---

### Task 4: Label Astro Server-Only Imports In The UI

**Files:**
- Modify: `extension/src/ui/format.ts`
- Modify: `extension/src/ui/inlayHints.ts`
- Modify: `extension/src/ui/decorations.ts`
- Modify: `extension/src/ui/codelens.ts`
- Modify: `extension/src/ui/tooltip.ts`
- Test: `extension/test/ui/format.test.ts`

- [ ] **Step 1: Write failing runtime label tests**

Add to `extension/test/ui/format.test.ts`:

```typescript
test("formatImportSize marks server-only imports", () => {
  assert.equal(
    formatImportSize(result, { display: "minimal", compression: "brotli", showWarnings: true }, "server"),
    "1.5 kB · server",
  );
});
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```powershell
pnpm test:ts
```

Expected: TypeScript fails because `formatImportSize()` accepts only two arguments.

- [ ] **Step 3: Add runtime-aware formatting**

In `extension/src/ui/format.ts`, import `ImportRuntime`:

```typescript
import type { ImportResult } from "../ipc/protocol.js";
import type { ImportRuntime } from "../imports/types.js";
```

Change `formatWarningSuffix()`:

```typescript
const formatWarningSuffix = (result: ImportResult, showWarnings: boolean, runtime: ImportRuntime): string => {
  if (runtime === "server") {
    return " · server";
  }

  if (result.is_cjs) {
    return " · CJS";
  }

  if (showWarnings && (result.side_effects || !result.truly_treeshakeable)) {
    return " · approximate";
  }

  return "";
};
```

Change `formatImportSize()`:

```typescript
export const formatImportSize = (
  result: ImportResult,
  options: FormatOptions,
  runtime: ImportRuntime = "component",
): string => {
  if (result.error) {
    return "unavailable";
  }

  if (options.display === "verbose" || options.compression === "all") {
    return `${formatBytes(result.brotli_bytes)} br · ${formatBytes(result.gzip_bytes)} gz · ${formatBytes(result.zstd_bytes)} zstd · ${formatBytes(result.minified_bytes)} min${formatWarningSuffix(result, options.showWarnings, runtime)}`;
  }

  const compressedBytes = bytesForCompression(result, options.compression);
  const compressed = formatBytes(compressedBytes);
  const label = labelForCompression(options.compression);
  const suffix = formatWarningSuffix(result, options.showWarnings, runtime);

  if (options.display === "minimal" || options.display === "inlayHint") {
    return `${compressed}${suffix}`;
  }

  return `${compressed} ${label} · ${formatBytes(result.minified_bytes)} min${suffix}`;
};
```

- [ ] **Step 4: Pass runtime from analysis state into UI labels**

In `extension/src/ui/inlayHints.ts`, change:

```typescript
formatImportSize(result, config)
```

to:

```typescript
formatImportSize(result, config, state.detected.runtime)
```

In `extension/src/ui/decorations.ts`, change:

```typescript
return formatImportSize(state.result, config);
```

to:

```typescript
return formatImportSize(state.result, config, state.detected.runtime);
```

In `extension/src/ui/codelens.ts`, change:

```typescript
title: formatImportSize(result, config),
```

to:

```typescript
title: formatImportSize(result, config, state.detected.runtime),
```

- [ ] **Step 5: Add runtime to tooltip details**

In `extension/src/ui/tooltip.ts`, import `ImportRuntime` and update `tooltipForResult()`:

```typescript
export const tooltipForResult = (
  result: ImportResult,
  runtime: ImportRuntime = "component",
): vscode.MarkdownString => {
```

Append runtime near the successful breakdown:

```typescript
tooltip.appendMarkdown(`Runtime: ${runtime}`);
```

Update call sites in `inlayHints.ts` and `decorations.ts` to pass `state.detected.runtime`.

- [ ] **Step 6: Verify runtime UI tests pass**

Run:

```powershell
pnpm test:ts
pnpm check
```

Expected: all TypeScript tests pass and `tsc --noEmit` exits 0.

- [ ] **Step 7: Commit**

```powershell
git add extension/src/ui/format.ts extension/src/ui/inlayHints.ts extension/src/ui/decorations.ts extension/src/ui/codelens.ts extension/src/ui/tooltip.ts extension/test/ui/format.test.ts
git commit -m "feat: label server-only component imports" -m "Carry component runtime metadata into ImportLens labels and hovers so Astro frontmatter imports are clearly marked as server-side instead of being confused with client bundle size."
```

---

### Task 5: Update Documentation And Requirements

**Files:**
- Modify: `docs/ImportLens-SRS.md`
- Modify: `README.md`

- [ ] **Step 1: Update SRS activation and import detection requirements**

In `docs/ImportLens-SRS.md`, update the startup sequence line that currently lists only JS/TS/React activation events to include `onLanguage:svelte` and `onLanguage:astro`.

Add a requirement under section `5.1 Import Detection and Syntax Handling`:

```markdown
**FR-006a** (High) - The extension must support Svelte component files by extracting JavaScript and TypeScript from `<script>` blocks before calling `oxc-parser`. Import positions returned to the UI must be mapped back to absolute positions in the original `.svelte` document so inlay hints, decorations, and hovers appear next to the import specifier.

**FR-006b** (High) - The extension must support Astro component files by extracting TypeScript frontmatter between the leading `---` fences and processed client `<script>` tags before calling `oxc-parser`. Astro frontmatter imports must be marked as server runtime in labels and hovers because Astro strips frontmatter code from browser output. Processed Astro client scripts must be marked as client runtime. Unprocessed Astro scripts such as `<script is:inline>` must be ignored because Astro does not transform or bundle their imports.
```

- [ ] **Step 2: Update README**

In `README.md`, add:

```markdown
ImportLens supports JavaScript, TypeScript, React/Solid JSX/TSX, Svelte component `<script>` blocks, and Astro frontmatter plus processed client scripts.
```

- [ ] **Step 3: Commit**

```powershell
git add docs/ImportLens-SRS.md README.md
git commit -m "docs: document component import support" -m "Update the SRS and README to specify Svelte and Astro script-region import detection, absolute-position mapping, and Astro server/client runtime labeling requirements."
```

---

### Task 6: Final Verification And Windows Package

**Files:**
- Modify: `extension/src/daemon/knownHashes.generated.ts` only if the packaging command rebuilds a changed daemon. For extension-only component parsing changes, the daemon hash should not change.

- [ ] **Step 1: Run full checks**

Run:

```powershell
pnpm check
pnpm test
cargo fmt --check
```

Expected:

- `pnpm check` exits 0.
- `pnpm test` reports all TypeScript and Rust tests passing.
- `cargo fmt --check` exits 0.

- [ ] **Step 2: Build the Windows VSIX**

Run:

```powershell
pnpm package:win32-x64
```

Expected:

- TypeScript bundle builds successfully.
- Windows VSIX is written to `import-lens-win32-x64-0.1.0.vsix`.
- Existing VSIX warnings about missing repository/LICENSE may remain until those metadata fields are added separately.

- [ ] **Step 3: Manual smoke test in VS Code**

Create or open a Svelte file with:

```svelte
<script lang="ts">
  import dayjs, { type Dayjs } from "dayjs";
  import utc from "dayjs/plugin/utc";
</script>
```

Expected:

- ImportLens activates when the Svelte document opens.
- Inlay hints appear after `"dayjs"` and `"dayjs/plugin/utc"`.
- The type-only `Dayjs` specifier does not produce a separate request.
- Hover shows the size breakdown, or `Copy diagnostics` if the daemon cannot compute a package.

Create or open an Astro file with:

```astro
---
import Icon from "astro-icon";
---

<Icon name="home" />

<script>
  import confetti from "canvas-confetti";
</script>
```

Expected:

- ImportLens activates when the Astro document opens.
- The frontmatter import shows a size label with a `server` suffix.
- The processed client script import shows a normal client/component size label.
- A `<script is:inline>` import does not produce an ImportLens hint.

- [ ] **Step 4: Commit package metadata if changed**

If `extension/src/daemon/knownHashes.generated.ts` changed unexpectedly, inspect why before committing. For extension-only changes, do not commit ignored artifacts such as `bin/`, `extension/dist/`, `target/`, or `*.vsix`.

---

## Follow-Up Plan For Other Component Formats

After Svelte and Astro are working, add remaining component extractors incrementally:

1. Vue: parse `<script>` and `<script setup>` blocks with `@vue/compiler-sfc`, support `lang="ts"`, add `onLanguage:vue`.
2. MDX: decide whether import extraction should be handled by a dedicated MDX-aware parser or a constrained top-of-file import scanner.

Each format should ship as its own task and commit with failing parser tests first.
