import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

// Drift check. The daemon decides which analysis stages are TRANSIENT — including engine loss,
// filesystem failures and compressor failure — and gates every cache it writes on that list. The extension has
// durable stores of its own (the persisted import-cost and bundle-impact histories) that have no
// TTL and no cache generation, so they must gate on exactly the SAME list — and the two cannot
// share a source, one being Rust and the other TypeScript.
//
// Add a transient stage to the daemon and forget the extension, and the extension records the
// fabricated size the daemon refused to cache, permanently. This is what fails then.

const repoFile = (relativePath) =>
  readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

const engineStages = repoFile("daemon/src/engine/mod.rs");
const pipelineStages = repoFile("daemon/src/pipeline/stage.rs");
const extensionTransience = repoFile("extension/src/analysis/transience.ts");
const cli = repoFile("cli/importlens.mjs");

const resolvedStage = (qualifiedConstant) => {
  const constant = qualifiedConstant.split("::").at(-1);
  const engineMacroDeclaration = new RegExp(`\\b${constant}\\s*=>\\s*"([^"]+)"`, "u").exec(
    engineStages,
  );
  const rustConstantDeclaration = new RegExp(
    `\\bpub const ${constant}: &str = "([^"]+)"`,
    "u",
  ).exec(`${engineStages}\n${pipelineStages}`);
  const resolved = engineMacroDeclaration ?? rustConstantDeclaration;
  assert.ok(resolved, `an engine or pipeline stage must declare ${qualifiedConstant}`);
  return resolved[1];
};

/** The product-wide transient stage constants, resolved to their wire strings. */
const daemonTransientStages = () => {
  const declaration = /pub const TRANSIENT_ANALYSIS_STAGES: &\[&str\] = &\[([^\]]*)\]/u.exec(
    pipelineStages,
  );
  assert.ok(
    declaration,
    "daemon/src/pipeline/stage.rs must still declare TRANSIENT_ANALYSIS_STAGES",
  );

  return declaration[1]
    .split(",")
    .map((constant) => constant.trim())
    .filter(Boolean)
    .map(resolvedStage)
    .sort();
};

const daemonDurableStages = () => {
  const declaration = /pub const DURABLE_RESULT_STAGES: &\[&str\] = &\[([^\]]*)\]/u.exec(
    pipelineStages,
  );
  assert.ok(declaration, "daemon/src/pipeline/stage.rs must declare DURABLE_RESULT_STAGES");
  const uncommented = declaration[1].replace(/\/\/[^\n]*/gu, "");
  return [...uncommented.matchAll(/\b(?:(?:engine_stage|diagnostic_stage)::)?[A-Z][A-Z_]+\b/gu)]
    .map((match) => match[0])
    .map(resolvedStage)
    .sort();
};

/** The stage strings the extension refuses to record. */
const extensionTransientStages = () => {
  const declaration = /transientAnalysisStages: readonly string\[\] = \[([^\]]*)\]/u.exec(
    extensionTransience,
  );
  assert.ok(declaration, "extension/src/analysis/transience.ts must still export the stage list");

  return [...declaration[1].matchAll(/"([^"]+)"/gu)].map((match) => match[1]).sort();
};

const extensionDurableStages = () => {
  const declaration = /durableResultStages: readonly string\[\] = \[([^\]]*)\]/u.exec(
    extensionTransience,
  );
  assert.ok(declaration, "extension/src/analysis/transience.ts must export the durable stage list");
  return [...declaration[1].matchAll(/"([^"]+)"/gu)].map((match) => match[1]).sort();
};

/** The stage strings the CI gate refuses to reach a verdict from. */
const cliTransientStages = () => {
  const declaration = /const transientStages = new Set\(\[([^\]]*)\]\)/u.exec(cli);
  assert.ok(declaration, "cli/importlens.mjs must still declare its transient stage set");

  return [...declaration[1].matchAll(/"([^"]+)"/gu)].map((match) => match[1]).sort();
};

const cliDurableStages = () => {
  const declaration = /const durableResultStages = new Set\(\[([^\]]*)\]\)/u.exec(cli);
  assert.ok(declaration, "cli/importlens.mjs must declare its durable stage set");
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

// The CI gate is the third copy, and the one that costs the most when it drifts: a pass/fail
// verdict is a durable store too (ADR-0006, invariant 3 — "or any pass/fail verdict"). A stage the
// daemon adds to `is_transient` and the CLI does not know about is a stage `importlens check`
// happily judges a budget from, or silently drops from a file total — which is defect #6, the one
// that merges the regression. The CLI ships standalone and cannot import the daemon or the
// extension, so the duplication is forced; this is what makes it safe.
test("the CI gate refuses to judge a budget from exactly the stages the daemon calls transient", () => {
  assert.deepEqual(
    cliTransientStages(),
    daemonTransientStages(),
    "a stage the daemon will not cache must be a stage `importlens check` will not pass on",
  );
});

test("measured-result stores mirror the daemon's durability allowlist", () => {
  const daemon = daemonDurableStages();
  assert.ok(daemon.length > 0, "the daemon must classify at least one durable result stage");
  assert.deepEqual(
    extensionDurableStages(),
    daemon,
    "the extension must default an unclassified measured diagnostic to refusal",
  );
  assert.deepEqual(
    cliDurableStages(),
    daemon,
    "the CLI must default an unclassified measured diagnostic to no verdict",
  );
});
