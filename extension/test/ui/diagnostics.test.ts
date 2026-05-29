import assert from "node:assert/strict";
import test from "node:test";
import { formatImportDiagnostics } from "../../src/ui/diagnostics.js";
import type { ImportResult } from "../../src/ipc/protocol.js";

const failedResult: ImportResult = {
  specifier: "@nestjs/common",
  raw_bytes: 0,
  minified_bytes: 0,
  gzip_bytes: 0,
  brotli_bytes: 0,
  zstd_bytes: 0,
  cache_hit: false,
  side_effects: true,
  truly_treeshakeable: false,
  is_cjs: false,
  error: "package entry not found near C:\\project\\node_modules\\@nestjs\\common\\missing",
  diagnostics: [
    {
      stage: "entry_resolution",
      message: "package entry not found near C:\\project\\node_modules\\@nestjs\\common\\missing",
      details: [
        "specifier: @nestjs/common",
        "package: @nestjs/common",
        "candidate: C:\\project\\node_modules\\@nestjs\\common\\missing.js",
      ],
    },
  ],
};

test("formatImportDiagnostics includes daemon error context without UI copy", () => {
  const formatted = formatImportDiagnostics(failedResult);

  assert.match(formatted, /ImportLens diagnostics for @nestjs\/common/u);
  assert.match(formatted, /\[entry_resolution\]/u);
  assert.match(formatted, /candidate: C:\\project\\node_modules\\@nestjs\\common\\missing\.js/u);
});
