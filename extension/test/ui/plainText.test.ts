import assert from "node:assert/strict";
import test from "node:test";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { plainTextFromMarkdown } from "../../src/ui/plainText.js";
import { tooltipForResultMarkdown } from "../../src/ui/tooltipMarkdown.js";

test("plain text drops markup, codicons and command links but keeps the words", () => {
  assert.equal(
    plainTextFromMarkdown(
      "**react**\n\n- Selected: **12.3 kB br**\n- $(warning) CJS\n\n[Copy diagnostics](command:importLens.copyImportDiagnostics?%5B%5D)\n\n[docs](https://example.com) for `react\\_dom`",
    ),
    "react\n\n- Selected: 12.3 kB br\n- CJS\n\ndocs for react_dom",
  );
});

test("a rendered import tooltip reads as plain text", () => {
  const result: ImportResult = {
    specifier: "react",
    raw_bytes: 30000,
    minified_bytes: 12000,
    gzip_bytes: 4000,
    brotli_bytes: 3500,
    zstd_bytes: 3800,
    cache_hit: true,
    side_effects: false,
    truly_treeshakeable: true,
    is_cjs: false,
    confidence: "high",
    confidence_reasons: [],
    error: null,
    diagnostics: [],
  };

  const text = plainTextFromMarkdown(tooltipForResultMarkdown(result, { compression: "brotli" }));

  assert.match(text, /^react\n/u);
  assert.doesNotMatch(text, /\*\*|\$\(|\]\(/u);
});
