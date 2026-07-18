#!/usr/bin/env node

// Validates a commit message against the project's conventional-commit rules:
// a `type(scope)!: subject` header (<=72 chars, no trailing period) plus a
// mandatory body. Machine-generated commits (merge/revert/fixup/squash) pass
// through. COMMIT_TYPES is the single source of truth and is asserted in tests
// to match cliff.toml, so the changelog grouping and this gate never drift.
//
// Usage: node scripts/check-commit-msg.mjs <path-to-commit-msg-file>
// Exit: 0 valid, 1 rejected, 2 usage error.

import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const COMMIT_TYPES = [
  "feat",
  "fix",
  "perf",
  "docs",
  "refactor",
  "style",
  "test",
  "chore",
  "ci",
  "build",
  // Undoing a change that shipped. It is its own type because it is its own EVENT: a reader
  // scanning history wants to see that something was taken back, and labelling it `fix` hides
  // that. The PASSTHROUGH below only covers git's own machine-generated `Revert "..."` subject,
  // which carries no scope and no explanation of why the revert was right.
  "revert",
];

const HEADER_MAX = 72;
const BODY_MIN_CHARS = 20;
const PASSTHROUGH = /^(Merge |Revert |fixup!|squash!)/u;

/** Drop git comment lines and everything from the `commit -v` scissors line. */
const stripNoise = (raw) => {
  const out = [];
  for (const line of raw.split("\n")) {
    if (/^# -+ >8 -+/u.test(line)) break;
    if (line.startsWith("#")) continue;
    out.push(line);
  }
  return out;
};

export const validateCommitMessage = (raw) => {
  const errors = [];
  const lines = stripNoise(raw ?? "");
  const header = (lines[0] ?? "").trimEnd();

  if (PASSTHROUGH.test(header)) {
    return { ok: true, errors };
  }

  const headerPattern = new RegExp(
    `^(${COMMIT_TYPES.join("|")})(\\([a-z0-9.-]+\\))?(!)?: (.+)$`,
    "u",
  );
  const match = header.match(headerPattern);
  if (!match) {
    errors.push(
      `Header must be '<type>(<scope>)!: <subject>' where type is one of: ${COMMIT_TYPES.join(", ")}.`,
    );
  } else {
    const subject = match[4];
    if (header.length > HEADER_MAX) {
      errors.push(`Header is ${header.length} chars; keep it <= ${HEADER_MAX}.`);
    }
    if (subject.endsWith(".")) {
      errors.push("Subject must not end with a period.");
    }
  }

  const hasBlankSeparator = lines.length <= 1 || lines[1].trim() === "";
  if (!hasBlankSeparator) {
    // Body is misplaced on line 2; report only the structural error, not a
    // spurious "body required" on top of it.
    errors.push("Leave a blank line between the header and the body.");
  } else {
    const body = lines.slice(2).join("\n").replace(/\s/gu, "");
    if (body.length < BODY_MIN_CHARS) {
      errors.push(
        `A commit body (description) is required — at least ${BODY_MIN_CHARS} non-whitespace characters explaining what and why.`,
      );
    }
  }

  return { ok: errors.length === 0, errors };
};

const main = () => {
  const file = process.argv[2];
  if (!file) {
    console.error("Usage: node scripts/check-commit-msg.mjs <path-to-commit-msg-file>");
    process.exit(2);
  }
  const { ok, errors } = validateCommitMessage(readFileSync(file, "utf8"));
  if (!ok) {
    console.error("✖ Commit message rejected:\n");
    for (const e of errors) {
      console.error(`  - ${e}`);
    }
    console.error(
      `\nFormat: <type>(<scope>)!: <subject>\\n\\n<body>\nTypes: ${COMMIT_TYPES.join(", ")}` +
        "\nBypass in a genuine emergency with --no-verify.",
    );
    process.exit(1);
  }
};

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  main();
}
