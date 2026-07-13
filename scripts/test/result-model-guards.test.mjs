import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

// GUARDS for the result model (docs/adr/0006-the-result-model.md).
//
// The same defect was found seven times in seven places, each only after the previous fix shipped,
// and every one of them was the same two lines of code: **a negative check on `error`** used as a
// stand-in for "is this result usable?", and **a missing size defaulted to zero**. Seven rounds of
// documenting the rule did not prevent the next instance. These fail instead.
//
// The compiler already does most of the work — a size is an `Option<u64>` / `number | null` now, so
// it cannot be read without asking. What the compiler cannot stop is someone answering the question
// wrongly: `.unwrap_or_default()`, `?? 0`, or reaching for `error` instead of the size.
//
// ## What changed here, and why
//
// This guard used to scan a hand-written list of 14 files, for one spelling (`!x.error`), on a
// hand-picked receiver name (`result` or `item`). Every one of those three is a place the NEXT
// crime scene can hide: it will be a file nobody added to the list, or a receiver called something
// else, or `.is_none()` instead of `!`. A guard that only fires on the mistakes already made is a
// record of history, not a defence.
//
// So the file set is DISCOVERED (every source file that consumes a size, 40-odd of them, not 14) and
// the patterns match the SHAPE, on any receiver.
//
// ## What it honestly cannot do
//
// It cannot tell `if (x.error === null)` used as a usability check from `x.error === null ||
// typeof x.error === "string"` used as a wire-shape validator (`ipc/client.ts` has nine of the
// latter). Deciding that needs types, not regex. So the `=== null` spelling is NOT banned, and this
// guard is not the whole defence — the RUNTIME invariant is: a size is `Option`/`null`-typed, so a
// consumer who asks `error` still cannot GET a size without asking for it separately, and every
// durable store applies its own gate at the insert (`ImportResult::is_durable`,
// `FileSizeComputation::is_cacheable`), where no static check has to reach.

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "../..");

const sourceRoots = ["daemon/src", "extension/src", "cli", "scripts"];
const sourceExtensions = new Set([".rs", ".ts", ".mjs", ".js"]);
const skippedDirectories = new Set(["node_modules", "dist", "target", "out"]);

const walk = (directory, found = []) => {
  for (const entry of readdirSync(directory)) {
    const full = path.join(directory, entry);
    if (statSync(full).isDirectory()) {
      if (!skippedDirectories.has(entry)) {
        walk(full, found);
      }
    } else if (sourceExtensions.has(path.extname(entry))) {
      found.push(full);
    }
  }
  return found;
};

/**
 * A file CONSUMES a size if it names one of the five measurements, handles an `ImportResult`, or
 * reaches for the sizes through the accessors. That is the population the rules are about, and it
 * is computed, not listed — so a new file that starts reading sizes is guarded the moment it does,
 * with nobody having to remember to add it here.
 */
const consumesASize =
  /\b(?:raw|minified|gzip|brotli|zstd)(?:_bytes|Bytes)\b|\bImportResult\b|measuredSizes|\.sizes\(\)/u;

const sizeConsumingFiles = sourceRoots
  .flatMap((root) => walk(path.join(repoRoot, root)))
  .map((full) => ({
    path: path.relative(repoRoot, full).split(path.sep).join("/"),
    text: readFileSync(full, "utf8"),
  }))
  // This file quotes every banned pattern in order to ban it.
  .filter((file) => file.path !== "scripts/test/result-model-guards.test.mjs")
  .filter((file) => consumesASize.test(file.text));

/**
 * The AGGREGATE — `FileSizeComputation`, `FileSizeDocumentResponse` — may legitimately ask about its
 * own `error`: its totals are plain `u64`, and `error` there means "nothing was summed at all". But
 * it may never ask about `error` ALONE, because the question that actually matters to a total ("was
 * an input missing?") has a different answer: `incomplete`. So the exemption is not a list of blessed
 * receivers — it is a demand that the expression consult `incomplete` too.
 *
 * `is_cacheable` (`self.error.is_none() && !self.incomplete && …`) and `isDurableFileSize`
 * (`!response.error && response.incomplete !== true && …`) both pass. `!result.error` on its own
 * never can, whatever the receiver is called.
 */
const consultsIncomplete = /incomplete/u;

const bannedShapes = [
  {
    name: "the negative `error` check as a usability test (JS/TS)",
    // `!x.error`, `!state.result.error`, `!x?.result?.error` — ANY receiver, any depth.
    pattern: /!\s*[\w$]+(?:(?:\?\.|\.)[\w$]+)*(?:\?\.|\.)error\b/u,
    exemptWhen: consultsIncomplete,
  },
  {
    name: "the negative `error` check as a usability test (Rust)",
    pattern: /\.error\s*\.\s*is_none\s*\(\)/u,
    exemptWhen: consultsIncomplete,
  },
  {
    name: "defaulting a missing size to zero (Rust)",
    // `brotli_bytes().unwrap_or_default()`, `.sizes().unwrap_or(...)`. Only the five MEASUREMENTS —
    // `shared_bytes.unwrap_or_default()` is fine, an absent shared count really is zero.
    pattern:
      /\b(?:raw|minified|gzip|brotli|zstd)_bytes(?:\(\))?\s*(?:\.\s*\w+\(\))*\s*\.\s*unwrap_or|\.sizes\(\)\s*(?:\.\s*\w+\(\))*\s*\.\s*unwrap_or/u,
  },
  {
    name: "defaulting a missing size to zero (TypeScript)",
    // `result.brotli_bytes ?? 0`, `row.brotliBytes || 0`.
    pattern: /\b(?:raw|minified|gzip|brotli|zstd)(?:_bytes|Bytes)\s*(?:\?\?|\|\|)\s*0\b/u,
  },
];

/** The statement `index` sits in — how far an `&&` chain can reach without a regex parsing Rust. */
const enclosingStatement = (text, index) => {
  const start = Math.max(
    ...[";", "{", "}"].map((token) => text.lastIndexOf(token, index)),
    text.lastIndexOf("\n\n", index),
  );
  const end = Math.min(
    ...[";", "{"].map((token) => {
      const found = text.indexOf(token, index);
      return found === -1 ? text.length : found;
    }),
  );
  return text.slice(Math.max(start, 0), end);
};

const isComment = (line) => /^\s*(?:\/\/|\*|\/\*|#)/u.test(line);

test("no size-consuming file asks whether there is an error instead of whether there is a size", () => {
  assert.ok(
    sizeConsumingFiles.length > 20,
    `discovery found only ${sizeConsumingFiles.length} size-consuming files; the walk is broken`,
  );

  const offences = [];

  for (const file of sizeConsumingFiles) {
    const lines = file.text.split(/\r?\n/u);
    let offset = 0;

    for (const [index, line] of lines.entries()) {
      const lineStart = offset;
      offset += line.length + 1;
      // A doc comment may quote a banned pattern to explain why it is banned; only code counts.
      if (isComment(line)) {
        continue;
      }

      for (const { name, pattern, exemptWhen } of bannedShapes) {
        const match = pattern.exec(line);
        if (!match) {
          continue;
        }
        if (exemptWhen?.test(enclosingStatement(file.text, lineStart + match.index))) {
          continue;
        }
        offences.push(`${file.path}:${index + 1} (${name}): ${line.trim()}`);
      }
    }
  }

  assert.deepEqual(
    offences,
    [],
    "a size exists if and only if a build succeeded, so ask for the SIZE (Option/`null`) and handle \
its absence - never `!result.error`, and never default it to zero. An AGGREGATE may check its own \
`error`, but only together with `incomplete`: a total can be short an input with nothing having \
failed",
  );
});

/**
 * The four build-derived durable stores. None of them may be handed an `ImportResult`.
 *
 * That is what makes them safe without a transience gate of their own: their only input is a
 * `BundleArtifact` / `ExportEnumeration`, which exists solely on the `Ok` side of a build, so a
 * failed build has nothing to give them. Plumb a result into one and this fails — which is the
 * moment to add the gate, not a review comment later.
 *
 * The stores that CAN be handed one — L1 memory, L2 disk, the L1 file-size aggregate — do not appear
 * here, because a static check is the wrong tool for them: they each apply the gate at the insert,
 * and `service.rs::every_durable_store_rejects_a_non_durable_outcome` proves it by feeding them one.
 */
const buildDerivedStores = [
  "daemon/src/pipeline/full_package.rs",
  "daemon/src/pipeline/export_list.rs",
  "daemon/src/pipeline/build_memo.rs",
  "daemon/src/engine/dependency_paths.rs",
].map((relative) => ({
  path: relative,
  text: readFileSync(path.join(repoRoot, relative), "utf8"),
}));

test("a build-derived store cannot be handed an ImportResult", () => {
  const offenders = buildDerivedStores
    .filter((file) => /\bImportResult\b/u.test(file.text))
    .map((file) => file.path);

  assert.deepEqual(
    offenders,
    [],
    "these stores are safe because a failure cannot REACH them - their input only exists when a \
build succeeded. A store that takes an ImportResult needs the transience gate every other store \
applies at its insert (ADR-0006, invariant 3)",
  );
});
