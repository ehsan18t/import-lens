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
