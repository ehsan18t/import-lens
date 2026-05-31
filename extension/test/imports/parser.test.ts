import assert from "node:assert/strict";
import test from "node:test";
import { extractRuntimeImports } from "../../src/imports/parser.js";

test("extractRuntimeImports handles static imports, re-exports, dynamic imports, and type-only skips", () => {
  const source = [
    "import React, { useMemo as memo } from 'react';",
    "import type { Foo } from 'bar';",
    "import * as dateFns from 'date-fns';",
    "export { z } from 'zod';",
    "const lazy = import('uuid');",
    "export { a, b } from 'pkg';",
  ].join("\n");

  const imports = extractRuntimeImports("sample.tsx", source);

  assert.deepEqual(
    imports.map((item) => ({
      specifier: item.specifier,
      kind: item.importKind,
      named: item.named,
      line: item.line,
    })),
    [
      { specifier: "react", kind: "default", named: [], line: 0 },
      { specifier: "react", kind: "named", named: ["useMemo"], line: 0 },
      { specifier: "date-fns", kind: "namespace", named: [], line: 2 },
      { specifier: "zod", kind: "named", named: ["z"], line: 3 },
      { specifier: "uuid", kind: "dynamic", named: [], line: 4 },
      { specifier: "pkg", kind: "named", named: ["a", "b"], line: 5 },
    ],
  );
});

test("extractRuntimeImports ignores non-literal dynamic imports", () => {
  const source = [
    "const packageName = 'react';",
    "const lazy = import(packageName);",
    "const templated = import(`pkg-${packageName}`);",
    "const literal = import('uuid');",
    "const staticTemplate = import(`date-fns`);",
  ].join("\n");

  const imports = extractRuntimeImports("sample.ts", source);

  assert.deepEqual(
    imports.map((item) => ({
      specifier: item.specifier,
      kind: item.importKind,
      line: item.line,
    })),
    [
      { specifier: "uuid", kind: "dynamic", line: 3 },
      { specifier: "date-fns", kind: "dynamic", line: 4 },
    ],
  );
});

test("extractRuntimeImports skips relative imports and Node builtins", () => {
  const source = [
    "import local from './local';",
    "import path from 'node:path';",
    "import fs from 'fs';",
    "import { debounce } from 'lodash-es';",
  ].join("\n");

  const imports = extractRuntimeImports("sample.ts", source);

  assert.deepEqual(imports.map((item) => item.specifier), ["lodash-es"]);
});

test("extractRuntimeImports keeps runtime default imports and skips mixed type specifiers", () => {
  const source = "import dayjs, { type Dayjs } from 'dayjs';";

  const imports = extractRuntimeImports("sample.ts", source);

  assert.deepEqual(
    imports.map((item) => ({
      specifier: item.specifier,
      kind: item.importKind,
      named: item.named,
    })),
    [{ specifier: "dayjs", kind: "default", named: [] }],
  );
});

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
