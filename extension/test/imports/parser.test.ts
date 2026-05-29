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
