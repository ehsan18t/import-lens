import assert from "node:assert/strict";
import test from "node:test";
import { createImportRequest } from "../../src/analysis/request.js";
import type { DetectedImport } from "../../src/imports/types.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

const detected: DetectedImport = detectedImport({
  specifier: "date-fns/format",
  packageName: "date-fns",
  named: ["format"],
  importKind: "named",
  quoteEnd: { line: 0, character: 31 },
  specifierRange: sourceRange(0, 8, 30),
  statementRange: sourceRange(0, 0, 33),
});

test("createImportRequest preserves subpath specifier and root package name", () => {
  assert.deepEqual(createImportRequest(detected, "3.6.0"), {
    specifier: "date-fns/format",
    package: "date-fns",
    version: "3.6.0",
    named: ["format"],
    import_kind: "named",
    runtime: "component",
  });
});

test("createImportRequest preserves detected import runtime", () => {
  assert.deepEqual(createImportRequest({ ...detected, runtime: "server" }, "3.6.0"), {
    specifier: "date-fns/format",
    package: "date-fns",
    version: "3.6.0",
    named: ["format"],
    import_kind: "named",
    runtime: "server",
  });
});
