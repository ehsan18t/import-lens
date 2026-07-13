import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

// Drift check. The daemon decides which engine failure stages are TRANSIENT — a build cancelled at
// its own deadline, one that unwound, one whose runtime went away — and gates every cache it writes
// on that list (`should_cache_result`, `FileSizeComputation::is_cacheable`). The extension has
// durable stores of its own (the persisted import-cost and bundle-impact histories) that have no
// TTL and no cache generation, so they must gate on exactly the SAME list — and the two cannot
// share a source, one being Rust and the other TypeScript.
//
// Add a transient stage to the daemon and forget the extension, and the extension records the
// fabricated size the daemon refused to cache, permanently. This is what fails then.

const repoFile = (relativePath) =>
  readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

const engineStages = repoFile("daemon/src/engine/mod.rs");
const extensionTransience = repoFile("extension/src/analysis/transience.ts");

/** The stage constants `stage::is_transient` matches on, resolved to their wire strings. */
const daemonTransientStages = () => {
  const matcher = /pub fn is_transient\(stage: &str\) -> bool \{\s*matches!\(stage,([^)]*)\)/u.exec(
    engineStages,
  );
  assert.ok(matcher, "daemon/src/engine/mod.rs must still declare stage::is_transient");

  return matcher[1]
    .split("|")
    .map((constant) => constant.trim())
    .filter(Boolean)
    .map((constant) => {
      const declaration = new RegExp(`\\b${constant}\\s*=>\\s*"([^"]+)"`, "u").exec(engineStages);
      assert.ok(declaration, `the stages! block must declare ${constant}`);
      return declaration[1];
    })
    .sort();
};

/** The stage strings the extension refuses to record. */
const extensionTransientStages = () => {
  const declaration = /transientEngineStages: readonly string\[\] = \[([^\]]*)\]/u.exec(
    extensionTransience,
  );
  assert.ok(declaration, "extension/src/analysis/transience.ts must still export the stage list");

  return [...declaration[1].matchAll(/"([^"]+)"/gu)].map((match) => match[1]).sort();
};

test("the extension refuses to persist exactly the stages the daemon calls transient", () => {
  const daemon = daemonTransientStages();

  assert.ok(daemon.length > 0, "the daemon must name at least one transient stage");
  assert.deepEqual(
    extensionTransientStages(),
    daemon,
    "a stage the daemon will not cache must be a stage the extension will not persist",
  );
});
