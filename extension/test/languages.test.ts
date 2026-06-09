import assert from "node:assert/strict";
import test from "node:test";
import { languageSelector, supportedLanguageIds } from "../src/languages.js";

test("supportedLanguageIds includes component document languages", () => {
  assert.equal(supportedLanguageIds.has("svelte"), true);
  assert.equal(supportedLanguageIds.has("astro"), true);
  assert.equal(supportedLanguageIds.has("vue"), true);
});

test("languageSelector is scoped to local file documents", () => {
  assert.deepEqual(languageSelector, [
    { language: "javascript", scheme: "file" },
    { language: "typescript", scheme: "file" },
    { language: "typescriptreact", scheme: "file" },
    { language: "javascriptreact", scheme: "file" },
    { language: "svelte", scheme: "file" },
    { language: "astro", scheme: "file" },
    { language: "vue", scheme: "file" },
  ]);
});
