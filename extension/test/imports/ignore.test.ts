import assert from "node:assert/strict";
import test from "node:test";
import { parseImportLensIgnore, shouldIgnoreImport } from "../../src/imports/ignore.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

const detected = (specifier: string) => detectedImport({
  specifier,
  packageName: specifier.startsWith("@") ? specifier.split("/").slice(0, 2).join("/") : specifier.split("/")[0],
  quoteEnd: { line: 0, character: 20 },
  specifierRange: sourceRange(0, 8, 18),
  statementRange: sourceRange(0, 0, 24),
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
