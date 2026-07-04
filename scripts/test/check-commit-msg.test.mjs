import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { COMMIT_TYPES, validateCommitMessage } from "../check-commit-msg.mjs";

const ok = (raw) => validateCommitMessage(raw).ok;

test("accepts a conventional header with a real body", () => {
  assert.equal(
    ok(
      "feat(hooks): add lefthook config\n\nWires pre-commit and pre-push jobs for both languages.",
    ),
    true,
  );
});

test("accepts an optional scope-less header and a breaking-change bang", () => {
  assert.equal(
    ok("fix: correct off-by-one\n\nThe loop dropped the final import; now inclusive."),
    true,
  );
  assert.equal(
    ok(
      "refactor(ipc)!: change the frame header\n\nBumps the wire protocol; documented in the SRS.",
    ),
    true,
  );
});

test("rejects an unknown type", () => {
  const { ok: pass, errors } = validateCommitMessage(
    "wip(x): stuff\n\nA sufficiently long body goes here.",
  );
  assert.equal(pass, false);
  assert.ok(errors.some((e) => /type/i.test(e)));
});

test("rejects a missing body (the description requirement)", () => {
  const { ok: pass, errors } = validateCommitMessage("docs: tidy readme");
  assert.equal(pass, false);
  assert.ok(errors.some((e) => /body/i.test(e)));
});

test("rejects a body that is only whitespace or too short", () => {
  assert.equal(ok("docs: tidy readme\n\n   "), false);
  assert.equal(ok("docs: tidy readme\n\ntoo short"), false);
});

test("rejects a subject with a trailing period", () => {
  assert.equal(
    ok("fix: do the thing.\n\nA sufficiently long body goes here to satisfy the rule."),
    false,
  );
});

test("rejects a header longer than 72 characters", () => {
  const longSubject = `feat: ${"x".repeat(80)}`;
  assert.equal(
    ok(`${longSubject}\n\nA sufficiently long body goes here to satisfy the rule.`),
    false,
  );
});

test("requires a blank line between header and body (and does not also claim the body is missing)", () => {
  const { ok: pass, errors } = validateCommitMessage(
    "feat: add thing\nBody with no blank line separator that is long enough.",
  );
  assert.equal(pass, false);
  assert.ok(errors.some((e) => /blank line/i.test(e)));
  assert.ok(!errors.some((e) => /body \(description\) is required/i.test(e)));
});

test("passes through merge, revert, fixup and squash commits", () => {
  assert.equal(ok('Merge branch "main" into feature'), true);
  assert.equal(ok('Revert "feat: add thing"\n\nThis reverts commit abc123.'), true);
  assert.equal(ok("fixup! feat: add thing"), true);
  assert.equal(ok("squash! feat: add thing"), true);
});

test("ignores comment lines and the commit -v scissors diff", () => {
  const raw = [
    "feat(hooks): add lefthook config",
    "",
    "Wires pre-commit and pre-push jobs for both languages.",
    "# Please enter the commit message for your changes.",
    "# ------------------------ >8 ------------------------",
    "diff --git a/x b/x",
    "+noise that must not count as body",
  ].join("\n");
  assert.equal(ok(raw), true);
});

test("COMMIT_TYPES stays in sync with cliff.toml commit_parsers", () => {
  const cliff = readFileSync(new URL("../../cliff.toml", import.meta.url), "utf8");
  for (const type of COMMIT_TYPES) {
    assert.ok(
      new RegExp(`\\^${type}\\b`).test(cliff),
      `cliff.toml has no parser for type '${type}'`,
    );
  }
});
