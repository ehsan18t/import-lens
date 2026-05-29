# Component Script Imports Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make ImportLens detect and size imports in Svelte component `<script>` blocks while preserving current JS/TS/React behavior.

**Architecture:** Extend the extension host, not the daemon. VS Code activation and document selectors must include Svelte, and import parsing must first split component documents into JavaScript/TypeScript script blocks, then feed those blocks to OXC with absolute source offsets so inlay hints still land after the original import specifier.

**Tech Stack:** TypeScript 6.x, VS Code extension APIs, `oxc-parser`, Node test runner, Rust daemon unchanged except for packaging hash refresh.

---

## Findings

1. `package.json` activates ImportLens only for `javascript`, `typescript`, `typescriptreact`, and `javascriptreact`; `svelte` files do not activate the extension.
2. `extension/src/config.ts` limits `supportedLanguageIds` and `languageSelector` to the same four language IDs, so even an already-active extension ignores Svelte documents in `DocumentAnalysisController.schedule()` and `analyze()`.
3. `extension/src/imports/parser.ts` sends the whole document buffer to `oxc-parser`. A full `.svelte` file is not valid JS/TS, so OXC recovery returns zero static imports for typical component markup.
4. The resolver and daemon are not the blocker. Once a Svelte import is detected with the original `.svelte` file path as `active_document_path`, existing package resolution and daemon sizing should work.

## Effort Estimate

- Svelte activation and selectors: **0.5 hour**
- Component script block extraction and absolute position mapping: **2 to 3 hours**
- Tests for Svelte scripts, mixed runtime/type imports, and language support: **1 to 1.5 hours**
- SRS/README updates, Windows package rebuild, and verification: **1 hour**
- Total for Svelte support: **4.5 to 6 hours**

Optional follow-up support:

- Vue `<script>` / `<script setup>`: **2 to 3 additional hours**
- Astro frontmatter: **1.5 to 2.5 additional hours**
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

test("supportedLanguageIds includes Svelte component documents", () => {
  assert.equal(supportedLanguageIds.has("svelte"), true);
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
]);

export const languageSelector: vscode.DocumentSelector = [
  { language: "javascript", scheme: "file" },
  { language: "typescript", scheme: "file" },
  { language: "typescriptreact", scheme: "file" },
  { language: "javascriptreact", scheme: "file" },
  { language: "svelte", scheme: "file" },
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

- [ ] **Step 5: Add Svelte activation**

In `package.json`, add:

```json
"onLanguage:svelte"
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
git commit -m "feat: register Svelte documents for analysis" -m "Add Svelte to ImportLens activation events, document selectors, and supported language filtering so component files can enter the analysis pipeline."
```

---

### Task 2: Extract Svelte Script Blocks

**Files:**
- Create: `extension/src/imports/scriptBlocks.ts`
- Modify: `extension/src/imports/parser.ts`
- Test: `extension/test/imports/scriptBlocks.test.ts`

- [ ] **Step 1: Write failing script block extraction tests**

Create `extension/test/imports/scriptBlocks.test.ts`:

```typescript
import assert from "node:assert/strict";
import test from "node:test";
import { scriptBlocksForDocument } from "../../src/imports/scriptBlocks.js";

test("scriptBlocksForDocument extracts Svelte TypeScript script content with absolute offset", () => {
  const source = [
    "<script lang=\"ts\">",
    "  import dayjs from 'dayjs';",
    "</script>",
    "<h1>{dayjs().format()}</h1>",
  ].join("\n");

  const blocks = scriptBlocksForDocument("Component.svelte", source);

  assert.equal(blocks.length, 1);
  assert.equal(blocks[0]?.language, "ts");
  assert.equal(blocks[0]?.source.trim(), "import dayjs from 'dayjs';");
  assert.equal(source.slice(blocks[0]?.offset ?? -1).startsWith("\n  import dayjs"), true);
});

test("scriptBlocksForDocument extracts module and instance Svelte scripts", () => {
  const source = [
    "<script context=\"module\">",
    "  import { browser } from '$app/environment';",
    "</script>",
    "<script>",
    "  import dayjs from 'dayjs';",
    "</script>",
  ].join("\n");

  const blocks = scriptBlocksForDocument("Component.svelte", source);

  assert.deepEqual(blocks.map((block) => block.language), ["js", "js"]);
  assert.equal(blocks.length, 2);
});

test("scriptBlocksForDocument keeps plain JavaScript documents as a single block", () => {
  const source = "import dayjs from 'dayjs';";

  const blocks = scriptBlocksForDocument("sample.ts", source);

  assert.deepEqual(blocks, [{ filename: "sample.ts", source, offset: 0, language: "ts" }]);
});
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```powershell
pnpm test:ts
```

Expected: TypeScript fails because `extension/src/imports/scriptBlocks.ts` does not exist.

- [ ] **Step 3: Implement script block extraction**

Create `extension/src/imports/scriptBlocks.ts`:

```typescript
import type { ParserOptions } from "oxc-parser";

export interface ScriptBlock {
  filename: string;
  source: string;
  offset: number;
  language: ParserOptions["lang"];
}

const svelteScriptPattern = /<script\b([^>]*)>([\s\S]*?)<\/script>/giu;

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

export const scriptBlocksForDocument = (filename: string, source: string): ScriptBlock[] => {
  if (!filename.endsWith(".svelte")) {
    return [{ filename, source, offset: 0, language: languageFromFilename(filename) }];
  }

  const blocks: ScriptBlock[] = [];

  for (const match of source.matchAll(svelteScriptPattern)) {
    const fullMatch = match[0];
    const attributes = match[1] ?? "";
    const scriptSource = match[2] ?? "";
    const matchIndex = match.index ?? 0;
    const contentOffset = matchIndex + fullMatch.indexOf(">") + 1;
    const language = languageFromAttributes(attributes);

    blocks.push({
      filename: blockFilename(filename, language, blocks.length),
      source: scriptSource,
      offset: contentOffset,
      language,
    });
  }

  return blocks;
};
```

- [ ] **Step 4: Verify script block tests pass**

Run:

```powershell
pnpm test:ts
```

Expected: script block tests pass; parser tests may still fail for Svelte imports until Task 3.

- [ ] **Step 5: Commit**

```powershell
git add extension/src/imports/scriptBlocks.ts extension/test/imports/scriptBlocks.test.ts
git commit -m "feat: extract Svelte script blocks" -m "Add a lightweight component script extractor that returns JavaScript and TypeScript blocks with absolute document offsets for Svelte components."
```

---

### Task 3: Parse Script Blocks With Absolute Positions

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
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```powershell
pnpm test:ts
```

Expected: Svelte parser tests fail because `extractRuntimeImports()` still parses the whole `.svelte` source and returns no imports.

- [ ] **Step 3: Refactor parser to parse blocks**

In `extension/src/imports/parser.ts`:

1. Import `ScriptBlock` and `scriptBlocksForDocument`.
2. Remove the local `languageFromFilename` function if Task 2 already moved that responsibility to `scriptBlocks.ts`.
3. Change `createDetectedImport()` to accept an offset and use absolute offsets.
4. Parse each script block separately and combine results.

Use this shape:

```typescript
import { scriptBlocksForDocument, type ScriptBlock } from "./scriptBlocks.js";
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
): DetectedImport => ({
  specifier,
  packageName: getPackageName(specifier),
  named: [...named].sort(),
  importKind,
  line: positionAt(documentSource, baseOffset + start).line,
  quoteEnd: positionAt(documentSource, baseOffset + quoteEndOffset),
  statementRange: rangeFromOffsets(documentSource, baseOffset + start, baseOffset + end),
});
```

Change parser helpers so they receive both `documentSource` and `block`, and call `createDetectedImport()` with `block.offset`.

Change `extractRuntimeImports()`:

```typescript
export const extractRuntimeImports = (filename: string, source: string): DetectedImport[] => {
  const imports: DetectedImport[] = [];

  for (const block of scriptBlocksForDocument(filename, source)) {
    imports.push(...extractRuntimeImportsFromBlock(source, block));
  }

  return imports.sort((left, right) => left.statementRange.start.line - right.statementRange.start.line);
};
```

Add `extractRuntimeImportsFromBlock()`:

```typescript
const extractRuntimeImportsFromBlock = (documentSource: string, block: ScriptBlock): DetectedImport[] => {
  const parsed = parseSync(block.filename, block.source, {
    ...parserOptions,
    lang: block.language,
  });
  const imports: DetectedImport[] = [];

  for (const item of parsed.module.staticImports) {
    imports.push(...importsFromStaticImport(documentSource, block, item));
  }

  for (const item of parsed.module.staticExports) {
    for (const entry of item.entries) {
      const detected = importFromStaticExport(documentSource, block, entry, item.start, item.end);

      if (detected) {
        imports.push(detected);
      }
    }
  }

  for (const item of parsed.module.dynamicImports) {
    const specifier = trimLiteralQuotes(block.source.slice(item.moduleRequest.start, item.moduleRequest.end));

    if (specifier && isRuntimePackageSpecifier(specifier)) {
      imports.push(createDetectedImport(documentSource, specifier, "dynamic", [], item.start, item.end, item.moduleRequest.end, block.offset));
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

Expected: all parser tests pass, including Svelte script imports with absolute line positions.

- [ ] **Step 5: Commit**

```powershell
git add extension/src/imports/parser.ts extension/test/imports/parser.test.ts
git commit -m "feat: parse imports from Svelte scripts" -m "Parse Svelte component script blocks with OXC and map import ranges back to absolute document positions so inlay hints and decorations render at the original import specifier."
```

---

### Task 4: Update Documentation And Requirements

**Files:**
- Modify: `docs/ImportLens-SRS.md`
- Modify: `README.md`

- [ ] **Step 1: Update SRS activation and import detection requirements**

In `docs/ImportLens-SRS.md`, update the startup sequence line that currently lists only JS/TS/React activation events to include `onLanguage:svelte`.

Add a requirement under section `5.1 Import Detection and Syntax Handling`:

```markdown
**FR-006a** (High) - The extension must support Svelte component files by extracting JavaScript and TypeScript from `<script>` blocks before calling `oxc-parser`. Import positions returned to the UI must be mapped back to absolute positions in the original `.svelte` document so inlay hints, decorations, and hovers appear next to the import specifier.
```

- [ ] **Step 2: Update README**

In `README.md`, add:

```markdown
ImportLens supports JavaScript, TypeScript, React JSX/TSX, and Svelte component `<script>` blocks.
```

- [ ] **Step 3: Commit**

```powershell
git add docs/ImportLens-SRS.md README.md
git commit -m "docs: document Svelte component support" -m "Update the SRS and README to specify Svelte script-block import detection and absolute-position mapping requirements."
```

---

### Task 5: Final Verification And Windows Package

**Files:**
- Modify: `extension/src/daemon/knownHashes.generated.ts` only if the packaging command rebuilds a changed daemon. For this Svelte-only extension change, the daemon hash should not change.

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

- [ ] **Step 4: Commit package metadata if changed**

If `extension/src/daemon/knownHashes.generated.ts` changed unexpectedly, inspect why before committing. For extension-only changes, do not commit ignored artifacts such as `bin/`, `extension/dist/`, `target/`, or `*.vsix`.

---

## Follow-Up Plan For Other Component Formats

After Svelte is working, add component extractors incrementally:

1. Vue: parse `<script>` and `<script setup>` blocks, support `lang="ts"`, add `onLanguage:vue`.
2. Astro: parse frontmatter between leading `---` fences, add `onLanguage:astro`.
3. MDX: decide whether import extraction should be handled by a dedicated MDX-aware parser or a constrained top-of-file import scanner.

Each format should ship as its own task and commit with failing parser tests first.

