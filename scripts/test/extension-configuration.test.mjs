import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const manifest = JSON.parse(readFileSync(new URL("../../package.json", import.meta.url), "utf8"));
const tsconfig = JSON.parse(readFileSync(new URL("../../tsconfig.json", import.meta.url), "utf8"));

test("Import Lens log level defaults to info for visible output-channel diagnostics", () => {
  assert.equal(
    manifest.contributes.configuration.properties["importLens.logLevel"].default,
    "info",
  );
});

test("Import Lens colored inline renderer is the default with native available", () => {
  const setting = manifest.contributes.configuration.properties["importLens.inlineRenderer"];

  assert.equal(setting.default, "colored");
  assert.deepEqual(setting.enum, ["native", "colored"]);
});

test("Import Lens keeps the VS Code engine aligned with the fork-compatible API baseline", () => {
  assert.equal(manifest.engines.vscode, "^1.90.0");
});

test("Import Lens TypeScript config targets the SRS baseline", () => {
  assert.equal(tsconfig.compilerOptions.target, "es2025");
});

test("Import Lens budgets expose per-import and per-file Brotli thresholds", () => {
  const setting = manifest.contributes.configuration.properties["importLens.budgets"];

  assert.deepEqual(setting.default, {});
  assert.equal(setting.properties.perImportBrotliBytes.minimum, 1);
  assert.equal(setting.properties.perFileBrotliBytes.minimum, 1);
  assert.equal(setting.additionalProperties, false);
});

test("Import Lens registry hints default on", () => {
  assert.equal(
    manifest.contributes.configuration.properties["importLens.enableRegistryHints"].default,
    true,
  );
});

test("Import Lens exposes cache retention policy settings", () => {
  const maxSize = manifest.contributes.configuration.properties["importLens.cacheMaxSizeMB"];
  const registryMaxSize =
    manifest.contributes.configuration.properties["importLens.registryCacheMaxSizeMB"];

  assert.equal(maxSize.default, 512);
  assert.equal(maxSize.minimum, 64);
  assert.equal(registryMaxSize.default, 32);
  assert.equal(registryMaxSize.minimum, 1);
});

test("Import Lens compare workflow is contributed and package.json can activate the extension", () => {
  assert.ok(manifest.activationEvents.includes("onLanguage:json"));
  assert.ok(manifest.activationEvents.includes("onLanguage:jsonc"));
  assert.ok(
    manifest.contributes.commands.some(
      (command) => command.command === "importLens.compareImports",
    ),
  );
});

test("Import Lens contributes cache management commands", () => {
  const commands = new Map(
    manifest.contributes.commands.map((command) => [command.command, command.title]),
  );

  assert.equal(commands.get("importLens.manageCache"), "Import Lens: Manage Cache");
  assert.equal(commands.get("importLens.clearCache"), "Import Lens: Clear Current Project Cache");
  assert.equal(commands.get("importLens.clearAllCaches"), "Import Lens: Clear All Caches");
});

test("Import Lens activates for Vue component files", () => {
  assert.ok(manifest.activationEvents.includes("onLanguage:vue"));
});
