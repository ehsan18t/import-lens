import assert from "node:assert/strict";
import test from "node:test";
import { classifyImportLensConfigChange } from "../src/configChange.js";

const event = (changed: string) => ({
  affectsConfiguration: (section: string): boolean => section === changed,
});

test("cache storage policy settings restart the daemon", () => {
  assert.equal(classifyImportLensConfigChange(event("importLens.cacheMaxSizeMB")), "daemonRestart");
});

test("a compression change re-reads the file size instead of only redrawing", () => {
  assert.equal(classifyImportLensConfigChange(event("importLens.compression")), "reanalyze");
});
