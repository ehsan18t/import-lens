import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

// GUARDS for the result model (docs/adr/0006-the-result-model.md).
//
// The same defect was found seven times in seven places, each only after the previous fix shipped,
// and every one of them was the same two lines of code: **an `error` check used as a stand-in for
// "is this result usable?"**, and **a missing size defaulted to zero**. Seven rounds of documenting
// the rule did not prevent the next instance. These fail instead.
//
// The compiler already does most of the work — a size is an `Option<u64>` / `number | null` now, so
// it cannot be read without asking. What the compiler cannot stop is someone answering the question
// wrongly: `.unwrap_or_default()`, `?? 0`, or reaching for `error` instead of the size.
//
// ## The two holes this guard used to have, and how they are closed
//
// **1. It only saw files that SPELL a size.** The discovery regex looked for `brotli_bytes`,
// `ImportResult`, `measuredSizes`. A TypeScript file that renders a size through a helper —
// `importResultSizeMarkdown(state.result, …)` — spells none of them, and `packageJsonTooltip.ts`
// and `inlayHints.ts` were live offenders in the canonical banned spelling, invisible to a scan of
// the 41 files the guard already knew about. (The previous review's "no live offender" finding
// scanned only those 41, which is circular.)
//
// Discovery is now by **what a file HANDLES, not what it names**: an `ImportResult`, or any type
// that carries one — `ImportAnalysisState { result?: ImportResult }`, and `AnalysisStore`, which
// holds those. The carrier set is COMPUTED to a fixpoint from the type declarations, so a new
// wrapper around a result is guarded the moment it exists, with nobody having to remember.
//
// **2. It matched one spelling of two.** `!x.error` was banned; the inverted and more idiomatic
// `if (x.error) { return }` — the *early exit* in front of the code that reads the size — was not.
// Rust likewise banned `.error.is_none()` and left `.error.is_some()`. Both polarities are banned
// now, in both languages.
//
// ## What it honestly cannot do, stated plainly
//
// It cannot tell `if (x.error === null)` used as a usability check from `x.error === null ||
// typeof x.error === "string"` used as a wire-shape validator (`ipc/client.ts` has nine of the
// latter). Deciding that needs types, not regex. So the `=== null` spelling is NOT banned.
//
// It also cannot tell an early-exit `error` gate from an error *reporter* — `if (result.error) {
// lines.push(\`Error: ${result.error}\`) }` is correct and must stay. The discriminator used here is
// whether the guarded block USES the error's value. That is a heuristic: a block that early-exits
// while quoting the message somewhere passes. It is not airtight and does not claim to be.
//
// **The runtime gate is what carries the weight**, and the ADR says so: a size is `Option`/`null`
// typed, so a consumer who asks `error` still cannot GET a size without asking for it separately,
// and every durable store applies its own gate at the insert (`ImportResult::is_durable`,
// `FileSizeComputation::is_cacheable`, `isDurableFileSize`), where no static check has to reach.

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

const allFiles = sourceRoots
  .flatMap((root) => walk(path.join(repoRoot, root)))
  .map((full) => ({
    path: path.relative(repoRoot, full).split(path.sep).join("/"),
    text: readFileSync(full, "utf8"),
  }))
  // This file quotes every banned pattern in order to ban it.
  .filter((file) => file.path !== "scripts/test/result-model-guards.test.mjs");

/**
 * A file consumes a size if it names one of the five measurements or reaches for them through the
 * accessors. The direct half of the population.
 */
const namesASize =
  /\b(?:raw|minified|gzip|brotli|zstd)(?:_bytes|Bytes)\b|\bImportResult\b|measuredSizes|\.sizes\(\)/u;

/**
 * Every TypeScript type that CARRIES an `ImportResult` — directly, or by holding another carrier.
 *
 * Computed to a fixpoint from the declarations, never listed. This is the half of the population the
 * old guard missed: a file that says `state.result` and hands it to a renderer names no size at all,
 * and it is exactly where the two live offenders were hiding.
 */
const resultCarryingTypes = () => {
  const declarations = [];
  for (const file of allFiles) {
    if (path.extname(file.path) !== ".ts") {
      continue;
    }
    const blocks = file.text.matchAll(
      /\b(?:interface|class)\s+([A-Z][\w]*)\b[^{]*\{([\s\S]*?)\n\}/gu,
    );
    const aliases = file.text.matchAll(/\btype\s+([A-Z][\w]*)\s*=\s*([^;]+);/gu);
    for (const match of [...blocks, ...aliases]) {
      declarations.push({ name: match[1], body: match[2] });
    }
  }

  const carriers = new Set(["ImportResult"]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const declaration of declarations) {
      if (carriers.has(declaration.name)) {
        continue;
      }
      const carriesOne = [...carriers].some((carrier) =>
        new RegExp(`\\b${carrier}\\b`, "u").test(declaration.body),
      );
      if (carriesOne) {
        carriers.add(declaration.name);
        grew = true;
      }
    }
  }
  return carriers;
};

const carrierPattern = new RegExp(`\\b(?:${[...resultCarryingTypes()].join("|")})\\b`, "u");

/** Files that NAME a size — the population the strictest rules apply to. */
const sizeNamingFiles = allFiles.filter((file) => namesASize.test(file.text));
/** Files that HANDLE a result, whether or not they ever spell a size. */
const resultHandlingFiles = allFiles.filter(
  (file) => namesASize.test(file.text) || carrierPattern.test(file.text),
);

/**
 * The AGGREGATE — `FileSizeComputation`, `FileSizeDocumentResponse` — may legitimately ask about its
 * own `error`: its totals are plain `u64`, and `error` there means "nothing was summed at all". But
 * it may never ask about `error` ALONE, because the question that actually matters to a total ("is
 * this number THIS FILE's?") has a different answer: `incomplete` (an under-count) and `degraded`
 * (an over-count). So the exemption is not a list of blessed receivers — it is a demand that the
 * expression consult them.
 */
const consultsTheAggregateFlags = /incomplete|degraded/u;

/**
 * An assertion is not a consumer. `assert!(totals.error.is_none())` in a test does not decide
 * whether anything is usable — it pins down the shape a test has deliberately constructed, and the
 * tests for invariant 4 have to say "error is None here, and the number is STILL not the file's".
 * Banning the spelling there would forbid stating the very thing under test.
 *
 * A stated limitation: production code could hide a real gate inside an `assert!`. Nothing here
 * would catch that. The runtime gates would.
 */
const isAnAssertion = (statement) => /\b(?:assert|debug_assert|expect)\b/u.test(statement);

/** The `{ … }` block that starts at or after `from`, brace-matched. */
const blockAfter = (text, from) => {
  const open = text.indexOf("{", from);
  if (open === -1) {
    return "";
  }
  let depth = 0;
  for (let index = open; index < text.length; index += 1) {
    if (text[index] === "{") {
      depth += 1;
    } else if (text[index] === "}") {
      depth -= 1;
      if (depth === 0) {
        return text.slice(open, index + 1);
      }
    }
  }
  return text.slice(open);
};

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

const negativeErrorCheck = {
  name: "the negative `error` check as a usability test (JS/TS)",
  // `!x.error`, `!state.result.error`, `!x?.result?.error` — ANY receiver, any depth. This spelling
  // is ONLY ever a usability test, so it is banned across every file that handles a result.
  pattern: /!\s*[\w$]+(?:(?:\?\.|\.)[\w$]+)*(?:\?\.|\.)error\b/u,
};

const rustErrorChecks = [
  {
    name: "the negative `error` check as a usability test (Rust)",
    pattern: /\.error\s*\.\s*is_none\s*\(\)/u,
  },
  {
    name: "the INVERTED `error` check as a usability test (Rust)",
    // The other polarity of the same mistake. It had no live offender; it had no guard either, and
    // "no offender yet" is what the six previous instances all had in common the day before.
    pattern: /\.error\s*\.\s*is_some\s*\(\)/u,
  },
];

test("no file that handles a result asks whether there is an error instead of whether there is a size", () => {
  assert.ok(
    sizeNamingFiles.length > 20,
    `discovery found only ${sizeNamingFiles.length} size-naming files; the walk is broken`,
  );
  assert.ok(
    resultHandlingFiles.length > sizeNamingFiles.length,
    "carrier discovery found nothing beyond the files that spell a size, which is the hole this \
guard exists to close - a file that renders a size through a HELPER names none of them",
  );

  const offences = [];

  for (const file of resultHandlingFiles) {
    const lines = file.text.split(/\r?\n/u);
    let offset = 0;

    for (const [index, line] of lines.entries()) {
      const lineStart = offset;
      offset += line.length + 1;
      // A doc comment may quote a banned pattern to explain why it is banned; only code counts.
      if (isComment(line)) {
        continue;
      }

      const match = negativeErrorCheck.pattern.exec(line);
      if (!match) {
        continue;
      }
      if (consultsTheAggregateFlags.test(enclosingStatement(file.text, lineStart + match.index))) {
        continue;
      }
      offences.push(`${file.path}:${index + 1} (${negativeErrorCheck.name}): ${line.trim()}`);
    }
  }

  assert.deepEqual(
    offences,
    [],
    "a size exists if and only if a build succeeded, so ask for the SIZE (Option/`null`) and handle \
its absence - never `!result.error`. An AGGREGATE may check its own `error`, but only together with \
`incomplete`/`degraded`: a total can be short an input, or be a sum of the wrong quantity, with \
nothing having failed",
  );
});

/**
 * An `if (…​.error)` that stands in FRONT of the code reading a size, rather than reporting the
 * error it just found.
 *
 * An `if (x.error)` that pushes the message into a tooltip, logs it, or throws it is correct and is
 * not this. The discriminator is whether the guarded block uses the value it just tested.
 */
const isAnErrorGate = (text, from) => {
  const block = blockAfter(text, from);
  const exitsEarly = /\b(?:return|continue|break)\b/u.test(block);
  const reportsTheError = /(?:\?\.|\.)error\b/u.test(block);
  return exitsEarly && !reportsTheError && !consultsTheAggregateFlags.test(block);
};

const inlineErrorGate = /\bif\s*\([^)]*?[\w$](?:\?\.|\.)error\s*\)/u;

const invertedOffencesIn = (file) => {
  const rustPatterns = path.extname(file.path) === ".rs" ? rustErrorChecks : [];
  const offences = [];
  let offset = 0;

  for (const [index, line] of file.text.split(/\r?\n/u).entries()) {
    const lineStart = offset;
    offset += line.length + 1;
    if (isComment(line)) {
      continue;
    }

    for (const { name, pattern } of rustPatterns) {
      const match = pattern.exec(line);
      const statement = match ? enclosingStatement(file.text, lineStart + match.index) : "";
      if (match && !consultsTheAggregateFlags.test(statement) && !isAnAssertion(statement)) {
        offences.push(`${file.path}:${index + 1} (${name}): ${line.trim()}`);
      }
    }

    const gate = inlineErrorGate.exec(line);
    if (gate && isAnErrorGate(file.text, lineStart + gate.index)) {
      offences.push(
        `${file.path}:${index + 1} (the INVERTED \`error\` check as a usability test): ${line.trim()}`,
      );
    }
  }

  return offences;
};

test("no size-naming file uses an `error` early-exit as the gate in front of a size", () => {
  const offences = sizeNamingFiles.flatMap(invertedOffencesIn);

  assert.deepEqual(
    offences,
    [],
    "`if (result.error) { return }` in front of a size read is the same mistake as `!result.error`, \
written the other way round - and it is worse, because it also waves through the shape that has \
neither an error nor a size: a still-LOADING import. Ask for the size",
  );
});

test("no file defaults a missing size to zero", () => {
  const bannedShapes = [
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

  const offences = [];

  for (const file of sizeNamingFiles) {
    for (const [index, line] of file.text.split(/\r?\n/u).entries()) {
      if (isComment(line)) {
        continue;
      }
      for (const { name, pattern } of bannedShapes) {
        if (pattern.test(line)) {
          offences.push(`${file.path}:${index + 1} (${name}): ${line.trim()}`);
        }
      }
    }
  }

  assert.deepEqual(
    offences,
    [],
    "a missing size is not a zero. Zero is a MEASUREMENT (a declarations-only package really does \
ship no runtime bytes); the absence of one is not, and defaulting it to zero is how a 17 kB package \
became a 0 B trend line",
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
 * The stores that CAN be handed one — L1 memory, L2 disk, the L1 file-size aggregate, and the
 * extension's two histories — do not appear here, because a static check is the wrong tool for them:
 * they each apply the gate at the insert, and a property test feeds each of them a real non-durable
 * outcome and asserts it kept nothing (`service.rs::every_durable_store_rejects_a_non_durable_outcome`,
 * `extension/test/analysis/transience.test.ts`).
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
