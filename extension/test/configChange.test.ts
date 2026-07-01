import assert from "node:assert/strict";
import test from "node:test";
import { classifyImportLensConfigChange } from "../src/configChange.js";

const event = (changed: string) => ({
  affectsConfiguration: (section: string): boolean => section === changed,
});

test("cache storage policy settings restart the daemon", () => {
  assert.equal(classifyImportLensConfigChange(event("importLens.cacheMaxSizeMB")), "daemonRestart");
  assert.equal(classifyImportLensConfigChange(event("importLens.cacheMaxAgeDays")), "daemonRestart");
});
