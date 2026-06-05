import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const manifest = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));

test("ImportLens log level defaults to info for visible output-channel diagnostics", () => {
  assert.equal(manifest.contributes.configuration.properties["importLens.logLevel"].default, "info");
});

test("ImportLens colored inline renderer is the default with native available", () => {
  const setting = manifest.contributes.configuration.properties["importLens.inlineRenderer"];

  assert.equal(setting.default, "colored");
  assert.deepEqual(setting.enum, ["colored", "native"]);
});

test("ImportLens budgets expose per-import and per-file Brotli thresholds", () => {
  const setting = manifest.contributes.configuration.properties["importLens.budgets"];

  assert.deepEqual(setting.default, {});
  assert.equal(setting.properties.perImportBrotliBytes.minimum, 1);
  assert.equal(setting.properties.perFileBrotliBytes.minimum, 1);
  assert.equal(setting.additionalProperties, false);
});
