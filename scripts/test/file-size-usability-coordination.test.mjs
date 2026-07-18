import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

// DRIFT. "Is this file's total a measurement of THIS FILE?" is one wire-visible quality rule, and
// three processes have to answer it: the daemon before it offers the total to its cache, the
// extension before it persists a bundle-impact row, and `importlens check` before it issues a CI
// verdict — which ADR-0006 invariant 3 calls a durable store like any other. The daemon cache also
// owns private fingerprint evidence that can make it stricter without changing the result's quality.
//
// It is written three times because it must be. The CLI ships standalone and cannot import the
// extension's TypeScript; neither can import the daemon's Rust. That is the same forced duplication
// as `stage::is_transient`, which has had a drift check since it was duplicated.
//
// This one did not, and here is what that cost. `degraded` — the file's own combined build failed,
// so the total is an un-deduplicated sum of per-import costs rather than a File Cost — was added to
// the daemon's gate and the extension's, and the CLI kept judging budgets against exactly that
// number. Every contributor Measured, `incomplete: false`, `error: null`, and a verdict drawn from a
// quantity ADR-0004 says is a different quantity. A budget judged against it is neither passed nor
// failed (invariant 5), and nothing failed when the third copy fell behind.
//
// So: extract the wire-visible fields each of the three consults, and demand they agree. Add one to
// any of them and forget the others, and this is red before the review is. The one cache-private
// input is named and guarded separately below rather than disappearing through a generic exception.

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "../..");

const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");

/** The text between `start` and the first `end` after it. */
const bodyBetween = (text, start, end) => {
  const from = text.indexOf(start);
  assert.notEqual(from, -1, `could not find "${start}" — the gate was renamed or moved`);
  const to = text.indexOf(end, from + start.length);
  assert.notEqual(to, -1, `could not find the end of "${start}"`);
  return text.slice(from + start.length, to);
};

/** Every field the gate reads off the totals it is judging. */
const fieldsRead = (body, receiver) =>
  new Set(
    [...body.matchAll(new RegExp(`\\b${receiver}\\s*\\.\\s*(\\w+)`, "gu"))].map(
      (match) => match[1],
    ),
  );

const daemonGate = bodyBetween(
  read("daemon/src/pipeline/file_size.rs"),
  "pub fn is_cacheable(&self) -> bool {",
  "\n    }",
);
const extensionGate = bodyBetween(
  read("extension/src/analysis/transience.ts"),
  "export const isDurableFileSize = (",
  ";\n",
);
const cliGate = bodyBetween(
  read("cli/importlens.mjs"),
  "export const isUsableFileSize = (response) =>",
  ";\n",
);

// The daemon's L1 cache has one additional, deliberately private admission input: exact file
// fingerprints captured while building. They are cache identity, not a quality field on the wire,
// so neither client can (or should) inspect them. Keep the exception named and singular: a generic
// "daemon-only" filter would let a future public field drift silently.
const daemonPrivateCacheFields = new Set(["dependency_fingerprints"]);

test("the daemon, the extension and the CLI judge a file's totals by the same fields", () => {
  const daemonFields = fieldsRead(daemonGate, "self");
  const daemonWireFields = new Set(
    [...daemonFields].filter((field) => !daemonPrivateCacheFields.has(field)),
  );
  const extensionFields = fieldsRead(extensionGate, "response");
  const cliFields = fieldsRead(cliGate, "response");

  // The daemon is the source of truth for wire-visible quality: it produces the totals, so it
  // decides what makes them untrustworthy. The other two must consult exactly those fields. Its
  // cache may additionally refuse a value whose private freshness evidence cannot be reused.
  assert.deepEqual(
    [...extensionFields].sort(),
    [...daemonWireFields].sort(),
    "extension/src/analysis/transience.ts::isDurableFileSize consults different wire-visible \
quality fields than daemon FileSizeComputation::is_cacheable",
  );
  assert.deepEqual(
    [...cliFields].sort(),
    [...daemonWireFields].sort(),
    "cli/importlens.mjs::isUsableFileSize consults different wire-visible quality fields than the \
daemon. The CLI issues a PASS/FAIL from this number, so it must interpret every public quality \
signal the daemon sends",
  );

  // And the set is not empty, so a gate emptied of every check cannot "agree" its way to green.
  assert.ok(
    daemonWireFields.has("error") &&
      daemonWireFields.has("incomplete") &&
      daemonWireFields.has("degraded"),
    `the daemon's gate must consult error, incomplete and degraded; it consults ${[...daemonWireFields]}`,
  );
});

test("the daemon's exact fingerprint gate stays enforced and private", () => {
  const daemonFields = fieldsRead(daemonGate, "self");
  assert.deepEqual(
    [...daemonFields].filter((field) => daemonPrivateCacheFields.has(field)),
    ["dependency_fingerprints"],
    "the one cache-private exception must remain explicit",
  );
  assert.match(
    daemonGate,
    /fingerprints_are_reusable\s*\(\s*&self\.dependency_fingerprints\s*\)/u,
    "the File Cost cache must refuse conflicted or unverifiable input snapshots",
  );

  const rustResponse = bodyBetween(
    read("daemon/src/ipc/protocol.rs"),
    "pub struct FileSizeDocumentResponse {",
    "\n}",
  );
  const typescriptResponse = bodyBetween(
    read("extension/src/ipc/protocol.ts"),
    "export interface FileSizeDocumentResponse {",
    "\n}",
  );
  assert.doesNotMatch(
    rustResponse,
    /dependency_fingerprints/u,
    "raw cache fingerprints must not become part of the Rust wire response",
  );
  assert.doesNotMatch(
    typescriptResponse,
    /dependency_fingerprints/u,
    "raw cache fingerprints must not become part of the TypeScript wire response",
  );
});

test("all three gates refuse a transient stage on the aggregate's own diagnostics", () => {
  // Not a field name, so the check above cannot see it: each gate scans the diagnostics for a
  // transient stage in its own idiom.
  assert.match(daemonGate, /is_transient/u, "the daemon's gate must scan for a transient stage");
  assert.match(
    extensionGate,
    /hasTransientStage/u,
    "the extension's gate must scan for a transient stage",
  );
  assert.match(
    cliGate,
    /transientStages\.has/u,
    "the CLI's gate must scan for a transient stage - a flaky agent must never report a REGRESSION \
either, and it must never report a pass",
  );
});

// DRIFT. One number, one set of WORDS for it.
//
// The daemon hands over a number and says what it is. Two processes then have to explain it to a
// human: the extension (status bar, "Show Current File Size") and `importlens check`. They said
// different things about the SAME number on the SAME run — the CLI printed "the file's combined
// build failed, so [it] is an un-deduplicated sum of its imports and not the file's size", and the
// status bar, on that response, said "File Cost - this file's imports built as one bundle": a
// confident, specific claim about the one mechanism that provably did not run.
//
// The sentences are written once, in `fileCostQuality.ts`, and mirrored in the CLI because the CLI
// ships standalone and can import no TypeScript - the same forced duplication as `isUsableFileSize`
// above. Reword one and forget the other, and this is red.
const qualityModel = read("extension/src/analysis/fileCostQuality.ts");

/** Every sentence the extension explains a non-durable total with. Derived, never typed out here. */
const sharedSentences = [
  ...bodyBetween(qualityModel, "export const fileCostBecause = (", "\n};").matchAll(
    /"([^"]{40,})"/gu,
  ),
].map((match) => match[1]);

test("the GUI and the CLI explain a total in the same words", () => {
  const cli = read("cli/importlens.mjs");

  assert.ok(
    sharedSentences.length >= 3,
    `only ${sharedSentences.length} sentences found in fileCostBecause; the extraction is broken, \
and a drift check that reads nothing agrees with everything`,
  );

  for (const sentence of sharedSentences) {
    assert.ok(
      cli.includes(sentence),
      `cli/importlens.mjs does not say what the extension says about the same number:\n  ${sentence}\n\
Two surfaces showing ONE figure and contradicting each other in words is the defect (ADR-0004).`,
    );
  }
});

// GUARD. One file budget, one number. The file budget is judged against a FILE COST - one bundle
// over all the file's imports, so a module two of them reach is counted once (ADR-0004) - and the
// only two surfaces that hold one are the editor (`file_size_document`) and `importlens check`.
//
// The workspace report held a second one. `apply_file_budget_warnings` summed each row's per-import
// brotli per source file and warned "File budget exceeded" off THAT: a Combined Import Cost, an
// upper bound, never a size. So the same file, under the same budget, was over budget in the report
// and inside it in the editor - and the report's number could not be fixed in place, because a
// report row is an import and has no combined build behind it.
//
// It is gone, and it stays gone: the report's unit is the import, and its per-import budget check
// (`is_import_budget_violation`) is genuinely per-import.
test("the workspace report runs no SECOND file budget off a sum of per-import costs", () => {
  const protocol = read("daemon/src/ipc/protocol.rs");
  const reportBudgets = bodyBetween(protocol, "pub struct WorkspaceReportBudgets {", "\n}");

  assert.doesNotMatch(
    reportBudgets,
    /per_file/u,
    "WorkspaceReportBudgets carries a per-file budget again - the report has no File Cost to judge \
it against, so whatever it judges is a sum of per-import costs and will contradict the editor and \
importlens check on the same file",
  );
  // The CODE, not the prose: the comments in these files explain at length why the file budget is
  // not here, and a guard that cannot be explained is a guard nobody keeps.
  assert.doesNotMatch(
    withoutComments(read("daemon/src/report/model.rs")),
    /file.?budget/iu,
    "the report model reaches a per-file budget verdict again (ADR-0004: its rows are imports, and \
their sum is a Combined Import Cost, not the file's size)",
  );
});

/** Rust source with `//`-comments removed, so a guard matches what the code DOES, not what it says. */
const withoutComments = (source) =>
  source
    .split("\n")
    .map((line) => line.replace(/\/\/.*$/u, ""))
    .join("\n");
