import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const manifest = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));

test("ImportLens log level defaults to info for visible output-channel diagnostics", () => {
  assert.equal(manifest.contributes.configuration.properties["importLens.logLevel"].default, "info");
});

test("ImportLens native inline renderer is the default with colored available", () => {
  const setting = manifest.contributes.configuration.properties["importLens.inlineRenderer"];

  assert.equal(setting.default, "native");
  assert.deepEqual(setting.enum, ["native", "colored"]);
});

test("ImportLens budgets expose per-import and per-file Brotli thresholds", () => {
  const setting = manifest.contributes.configuration.properties["importLens.budgets"];

  assert.deepEqual(setting.default, {});
  assert.equal(setting.properties.perImportBrotliBytes.minimum, 1);
  assert.equal(setting.properties.perFileBrotliBytes.minimum, 1);
  assert.equal(setting.additionalProperties, false);
});

test("ImportLens registry hints are opt-in", () => {
  assert.equal(manifest.contributes.configuration.properties["importLens.enableRegistryHints"].default, false);
});

test("ImportLens compare workflow is contributed and package.json can activate the extension", () => {
  assert.ok(manifest.activationEvents.includes("onLanguage:json"));
  assert.ok(manifest.contributes.commands.some((command) => command.command === "importLens.compareImports"));
});
