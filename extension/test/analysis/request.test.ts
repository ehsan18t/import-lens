import assert from "node:assert/strict";
import test from "node:test";
import { createImportRequest } from "../../src/analysis/request.js";
import type { DetectedImport } from "../../src/imports/types.js";

const detected: DetectedImport = {
  specifier: "date-fns/format",
  packageName: "date-fns",
  named: ["format"],
  importKind: "named",
  runtime: "component",
  line: 0,
  quoteEnd: { line: 0, character: 31 },
  statementRange: {
    start: { line: 0, character: 0 },
    end: { line: 0, character: 33 },
  },
};

test("createImportRequest preserves subpath specifier and root package name", () => {
  assert.deepEqual(createImportRequest(detected, "3.6.0"), {
    specifier: "date-fns/format",
    package: "date-fns",
    version: "3.6.0",
    named: ["format"],
    import_kind: "named",
  });
});
