import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

// DRIFT. "Is this file's total a measurement of THIS FILE?" is one rule, and three processes have to
// answer it: the daemon before it caches the total, the extension before it persists a bundle-impact
// row, and `importlens check` before it issues a CI verdict — which ADR-0006 invariant 3 calls a
// durable store like any other.
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
// So: extract the fields each of the three consults, and demand they agree. Add one to any of them
// and forget the others, and this is red before the review is.

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

test("the daemon, the extension and the CLI judge a file's totals by the same fields", () => {
  const daemonFields = fieldsRead(daemonGate, "self");
  const extensionFields = fieldsRead(extensionGate, "response");
  const cliFields = fieldsRead(cliGate, "response");

  // The daemon is the source of truth: it produces the totals, so it decides what makes them
  // untrustworthy. The other two must consult exactly what it consults.
  assert.deepEqual(
    [...extensionFields].sort(),
    [...daemonFields].sort(),
    "extension/src/analysis/transience.ts::isDurableFileSize consults a different set of fields \
than daemon FileSizeComputation::is_cacheable. One of them will accept a total the other refuses",
  );
  assert.deepEqual(
    [...cliFields].sort(),
    [...daemonFields].sort(),
    "cli/importlens.mjs::isUsableFileSize consults a different set of fields than the daemon's \
FileSizeComputation::is_cacheable. The CLI issues a PASS/FAIL from this number - a gate weaker than \
the cache's means CI judges a total the daemon would not even keep for 30 seconds",
  );

  // And the set is not empty, so a gate emptied of every check cannot "agree" its way to green.
  assert.ok(
    daemonFields.has("error") && daemonFields.has("incomplete") && daemonFields.has("degraded"),
    `the daemon's gate must consult error, incomplete and degraded; it consults ${[...daemonFields]}`,
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
