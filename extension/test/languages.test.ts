import assert from "node:assert/strict";
import test from "node:test";
import { supportedLanguageIds } from "../src/languages.js";

test("supportedLanguageIds includes Svelte and Astro component documents", () => {
  assert.equal(supportedLanguageIds.has("svelte"), true);
  assert.equal(supportedLanguageIds.has("astro"), true);
});
