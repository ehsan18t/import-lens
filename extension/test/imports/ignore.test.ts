import assert from "node:assert/strict";
import test from "node:test";
import { parseImportLensIgnore, shouldIgnoreImport } from "../../src/imports/ignore.js";
import type { DetectedImport } from "../../src/imports/types.js";

const detected = (specifier: string): DetectedImport => ({
  specifier,
  packageName: specifier.startsWith("@") ? specifier.split("/").slice(0, 2).join("/") : specifier.split("/")[0],
  named: [],
  importKind: "namespace",
  syntax: "static",
  runtime: "component",
  line: 0,
  quoteEnd: { line: 0, character: 20 },
  statementRange: {
    start: { line: 0, character: 0 },
    end: { line: 0, character: 24 },
  },
});

test("parseImportLensIgnore supports package, import, and path rules", () => {
  const rules = parseImportLensIgnore([
    "# comment",
    "package:moment",
    "import:@internal/*",
    "path:src/generated/**",
    "",
  ].join("\n"));

  assert.equal(shouldIgnoreImport(detected("moment"), "/workspace/src/app.ts", rules), true);
  assert.equal(shouldIgnoreImport(detected("@internal/ui"), "/workspace/src/app.ts", rules), true);
  assert.equal(shouldIgnoreImport(detected("react"), "/workspace/src/generated/types.ts", rules), true);
  assert.equal(shouldIgnoreImport(detected("react"), "/workspace/src/app.ts", rules), false);
});
