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
// ## WHAT THIS GUARD ACTUALLY COVERS — the honest number, and how it was measured
//
// A regex over source text is not a parser, and this one is **not** a complete ban on the banned
// idea. It catches **18 of the 24** spellings in `BANNED_SPELLINGS` below, and `the guard catches
// exactly the spellings it claims to` **pins that number** by computing it: widen or weaken any
// matcher and the test fails until the claim is raised with it.
//
// **The corpus is not the universe, and a corpus you tuned against measures nothing.** The first 18
// spellings were planted by a reviewer, and successive versions of this guard scored 6, then 14, then
// 15 against them. But an *independent* set of 13 fresh spellings — planted afterwards, against the
// same matchers — scored **3 of 13**. That is the number that tells the truth about reach, and it is
// why the six best of those plants have been folded into the corpus (they now score 6 of 13; the
// Rust half is 5 of 5, and the TypeScript half is 1 of 8).
//
// What still gets through is **structural, not an oversight**, and no amount of regex fixes it:
// destructuring or aliasing the field (`const { error } = result`) throws the receiver away; a bare
// `x.error == null` in an expression is indistinguishable from the nine legitimate wire-shape
// validators in `ipc/client.ts`; and a ternary is how a correct error *reporter* is written too.
// Telling those apart needs types. A guard that looks stronger than it is, is worse than no guard,
// because it buys false confidence — which is precisely how this defect survived seven rounds.
//
// **So the runtime gate is what ENFORCES the rule; this only catches the spellings it can see.** A
// size is `Option`/`null`-typed, so a consumer who asks about `error` still cannot GET a size without
// asking for it separately — and every durable store applies its own gate at the insert
// (`ImportResult::is_durable`, `FileSizeComputation::is_cacheable`, `isDurableFileSize`,
// `isUsableFileSize`), where no static check has to reach. Those gates are quantified over by
// property tests that derive the whole non-durable stage vocabulary FROM the allowlist and feed each
// store a real non-durable outcome, asserting it kept nothing
// (`service.rs::every_durable_store_rejects_a_non_durable_outcome`,
// `extension/test/analysis/transience.test.ts`). That is the enforcement. This is the tripwire.
//
// ## The three holes that ARE closed, and were not
//
// **1. It only saw files that SPELL a size.** The discovery regex looked for `brotli_bytes`,
// `ImportResult`, `measuredSizes`. A TypeScript file that renders a size through a helper —
// `importResultSizeMarkdown(state.result, …)` — spells none of them, and `packageJsonTooltip.ts`
// and `inlayHints.ts` were live offenders in the canonical banned spelling, invisible to a scan of
// the 41 files the guard already knew about.
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
// **3. A COMMENT SILENCED IT.** The scan skipped any line whose first token was `//`, `*` or `/*` —
// so `/* size */ if (!result.error) { … }` was invisible, and so was every offence hidden behind a
// leading block-comment fragment. Comments are now *stripped* (blanked in place, preserving line and
// column offsets) rather than used to skip whole lines, so a comment can no longer hide code.
//
// ## What it still cannot do, stated plainly rather than papered over
//
// It cannot tell `if (x.error === null)` used as a usability check from `x.error === null ||
// typeof x.error === "string"` used as a wire-shape validator (`ipc/client.ts` has nine of the
// latter). Deciding that needs types, not regex. So a `=== null` COMPARISON is banned only where it
// gates an early exit in front of a size read, and never in an expression.
//
// It also cannot tell an early-exit `error` gate from an error *reporter* — `if (result.error) {
// lines.push(\`Error: ${result.error}\`) }` is correct and must stay. The discriminator used here is
// whether the guarded block USES the error's value. That is a heuristic: a block that early-exits
// while quoting the message somewhere passes. It is not airtight and does not claim to be.

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

/** Where the Rust raw string at `index` ends (`r"…"`, `r#"…"#`), or -1 if one does not start there. */
const endOfRawString = (text, index) => {
  let hashes = 0;
  while (text[index + 1 + hashes] === "#") {
    hashes += 1;
  }
  if (text[index + 1 + hashes] !== '"') {
    return -1;
  }
  const terminator = `"${"#".repeat(hashes)}`;
  const end = text.indexOf(terminator, index + 2 + hashes);
  return end === -1 ? text.length : end + terminator.length;
};

/** Where the string literal at `index` ends, or -1 if one does not start there. */
const endOfLiteral = (text, index) => {
  const char = text[index];
  if (char === "r" && !/[\w$]/u.test(text[index - 1] ?? "")) {
    return endOfRawString(text, index);
  }
  if (char !== '"' && char !== "'" && char !== "`") {
    return -1;
  }

  let scan = index + 1;
  while (scan < text.length && text[scan] !== char) {
    scan += text[scan] === "\\" ? 2 : 1;
  }
  return scan + 1;
};

/** Where the comment at `index` ends, or -1 if one does not start there. */
const endOfComment = (text, index) => {
  if (text[index] !== "/") {
    return -1;
  }
  if (text[index + 1] === "/") {
    const end = text.indexOf("\n", index);
    return end === -1 ? text.length : end;
  }
  if (text[index + 1] === "*") {
    const end = text.indexOf("*/", index + 2);
    return end === -1 ? text.length : end + 2;
  }
  return -1;
};

/**
 * Every comment blanked to spaces, with newlines kept — so line and column offsets are unchanged and
 * a match still reports where it really is.
 *
 * This replaces the old "skip a line that STARTS with a comment token" rule, which a leading block
 * comment silenced completely: `/* size *\/ if (!result.error) { … }` was invisible to it, and so was
 * an aggregate exemption satisfied by PROSE inside the guarded block rather than by code. String and
 * template literals are tracked (a URL is not a comment), as are Rust raw strings.
 */
const stripComments = (text) => {
  const out = [...text];
  let index = 0;

  while (index < text.length) {
    const literal = endOfLiteral(text, index);
    if (literal !== -1) {
      index = literal;
      continue;
    }

    const comment = endOfComment(text, index);
    if (comment === -1) {
      index += 1;
      continue;
    }

    for (let scan = index; scan < comment; scan += 1) {
      if (out[scan] !== "\n") {
        out[scan] = " ";
      }
    }
    index = comment;
  }

  return out.join("");
};

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
 * (an over-count).
 *
 * So the exemption is not a list of blessed receivers, and it is no longer a demand that the
 * *statement* name the flags either — that version was satisfied by a **comment**, and an early-exit
 * gate consults the flags after the exit by construction, never inside it. It is a demand on the
 * RECEIVER: the same object must be asked `.incomplete` or `.degraded` somewhere in the same file.
 * Only an aggregate has those fields, in either language, so a per-import result cannot buy the
 * exemption without a field that does not compile.
 */
const consultsTheAggregateFlags = (code, receiver) => {
  const chain = receiver
    .split(/\s*\??\.\s*/u)
    .filter(Boolean)
    .map((part) => part.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&"))
    .join("\\s*\\??\\.\\s*");
  return new RegExp(`\\b${chain}\\s*\\??\\.\\s*(?:incomplete|degraded)\\b`, "u").test(code);
};

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

/** The line `index` sits on, 1-based. */
const lineAt = (text, index) => text.slice(0, index).split("\n").length;

/** The source line containing `index`, trimmed — for the offence report. */
const lineTextAt = (text, index) => {
  const start = text.lastIndexOf("\n", index) + 1;
  const end = text.indexOf("\n", index);
  return text.slice(start, end === -1 ? text.length : end).trim();
};

/**
 * The body an `if (…)` guards: the brace-matched block, or — for a braceless `if (x) return;` — the
 * single statement up to its `;`. The old version always reached for the next `{` in the FILE, so a
 * braceless early exit was judged against some unrelated block further down.
 */
const guardedBody = (text, from) => {
  let index = text.indexOf("(", from);
  let depth = 0;
  for (; index < text.length; index += 1) {
    if (text[index] === "(") {
      depth += 1;
    } else if (text[index] === ")") {
      depth -= 1;
      if (depth === 0) {
        index += 1;
        break;
      }
    }
  }

  while (index < text.length && /\s/u.test(text[index])) {
    index += 1;
  }

  if (text[index] !== "{") {
    const end = text.indexOf(";", index);
    return text.slice(index, end === -1 ? text.length : end + 1);
  }

  depth = 0;
  for (let scan = index; scan < text.length; scan += 1) {
    if (text[scan] === "{") {
      depth += 1;
    } else if (text[scan] === "}") {
      depth -= 1;
      if (depth === 0) {
        return text.slice(index, scan + 1);
      }
    }
  }
  return text.slice(index);
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

/**
 * The `if (` whose CONDITION `index` sits in — walking outward through any call parentheses in the
 * way (`if (Boolean(result.error))`) — or -1.
 */
const enclosingIf = (code, index) => {
  let depth = 0;
  for (let scan = index; scan >= 0; scan -= 1) {
    const char = code[scan];
    if (char === ")") {
      depth += 1;
    } else if (char === "(") {
      if (depth === 0) {
        if (/\bif\s*$/u.test(code.slice(Math.max(0, scan - 8), scan))) {
          return scan;
        }
        continue;
      }
      depth -= 1;
    } else if (depth === 0 && (char === ";" || char === "{" || char === "}")) {
      return -1;
    }
  }
  return -1;
};

/** Every `<receiver>.error` / `<receiver>?.error` read, with the receiver it was asked of. */
const errorReads = /(?<![\w$])([\w$]+(?:\s*(?:\?\.|\.)\s*[\w$]+)*)\s*(?:\?\.|\.)\s*error\b/gu;

/**
 * Every banned spelling of "is this result usable?" — found by scanning each `.error` READ and
 * asking what is being done with it, rather than by matching each idiom's surface text, which is how
 * two thirds of `BANNED_SPELLINGS` used to get through.
 *
 * Three shapes are refused:
 *
 * * **negation** — `!result.error`, `!(result.error)`, `!state.result?.error`, across line breaks.
 *   This spelling is ONLY ever a usability test. (The `!` must not be a Rust macro's: `assert!(…)`.)
 * * **an `error`-shaped question in Rust** — `.is_none()` / `.is_some()` (both polarities; the second
 *   had no live offender and no guard either, and "no offender yet" is what the six previous
 *   instances all had in common the day before), through `as_ref()`, and written as a match
 *   (`matches!(x.error, None)`, `x.error == None`).
 * * **an early-exit gate** — `if (result.error) { return }` standing in FRONT of the code that reads
 *   the size, plain or compared against `null`/`undefined`. Worse than `!result.error`, because it
 *   also waves through the shape that has neither an error nor a size: a still-LOADING import. An
 *   `if (x.error)` that pushes the message into a tooltip, logs it, or throws it is a *reporter*,
 *   is correct, and must stay — the discriminator is whether the guarded block uses the value it
 *   just tested.
 */
/** `.is_none()` / `.is_some()` / `matches!(…, None)` / `== None` — an `error`-shaped question. */
const rustOffence = (before, after) => {
  if (/^\s*(?:\.\s*as_ref\s*\(\)\s*)?\.\s*is_none\s*\(\)/u.test(after)) {
    return "the negative `error` check as a usability test (Rust)";
  }
  if (/^\s*(?:\.\s*as_ref\s*\(\)\s*)?\.\s*is_some\s*\(\)/u.test(after)) {
    return "the INVERTED `error` check as a usability test (Rust)";
  }
  if (/^\s*[=!]=\s*None\b/u.test(after) || /matches!\s*\(\s*$/u.test(before)) {
    return "the `error` check written as a match (Rust)";
  }
  // A PATTERN MATCH, not a method call: `let Some(_) = x.error else`, `if let Some(_) = x.error`,
  // `match x.error { Some(_) => … }`. Rust's most idiomatic way to ask the banned question, and the
  // guard used to miss all three — an independent probe planted them and walked straight through.
  if (/(?:\bif\s+)?\blet\s+Some\s*\([^)]*\)\s*=\s*$/u.test(before) || /\bmatch\s+$/u.test(before)) {
    return "the `error` check written as a pattern match (Rust)";
  }
  // `.map_or(true, |_| false)`, `.is_some_and(…)`, `.is_none_or(…)` — the same question wearing a
  // combinator.
  if (
    /^\s*(?:\.\s*as_ref\s*\(\)\s*)?\.\s*(?:map_or|map_or_else|is_some_and|is_none_or)\s*\(/u.test(
      after,
    )
  ) {
    return "the `error` check written as a combinator (Rust)";
  }
  return null;
};

/** `if (result.error) { return }` in front of the size read — the early exit, not a reporter. */
const gateOffence = (code, index, after) => {
  const gate = enclosingIf(code, index);
  if (gate === -1 || !/^\s*(?:[=!]==?\s*(?:null|undefined)\s*)?\)/u.test(after)) {
    return null;
  }

  const body = guardedBody(code, gate);
  const exitsEarly = /\b(?:return|continue|break)\b/u.test(body);
  const reportsTheError = /(?:\?\.|\.)error\b/u.test(body);
  return exitsEarly && !reportsTheError ? "the INVERTED `error` check as a usability test" : null;
};

const scan = (file, { gates }) => {
  const code = stripComments(file.text);
  const isRust = path.extname(file.path) === ".rs";
  const offences = [];

  for (const match of code.matchAll(errorReads)) {
    const [read, receiver] = match;
    const before = code.slice(0, match.index);
    const after = code.slice(match.index + read.length);

    // An aggregate may ask about its own `error` — but only if the same object is also asked the two
    // questions that actually decide whether its number is this file's.
    if (consultsTheAggregateFlags(code, receiver)) {
      continue;
    }

    const negated = /(?<![\w$])!\s*\(*\s*$/u.test(before);
    const rust =
      gates && isRust && !isAnAssertion(enclosingStatement(code, match.index))
        ? rustOffence(before, after)
        : null;
    const name =
      (negated ? "the negative `error` check as a usability test" : null) ??
      rust ??
      (gates ? gateOffence(code, match.index, after) : null);

    if (name) {
      offences.push(
        `${file.path}:${lineAt(code, match.index)} (${name}): ${lineTextAt(file.text, match.index)}`,
      );
    }
  }

  return offences;
};

/**
 * The two populations, and why they differ.
 *
 * **Negation** is banned wherever a result is HANDLED, because `!x.error` is only ever a usability
 * test — there is no other reason to write it.
 *
 * The **gate** and the Rust spellings are banned only where a size is NAMED, because they are the
 * shape "an `error` check standing in front of a size read", and the same shape in front of a
 * package.json partial or a cache-status response — neither of which has a size at all — is the only
 * correct thing to write (`ui/cacheManager.ts`, `guidance/packageJsonAnalysis.ts`).
 */
const negativeOffencesIn = (file) => scan(file, { gates: false });
const gateOffencesIn = (file) => scan(file, { gates: true });

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

  assert.deepEqual(
    resultHandlingFiles.flatMap(negativeOffencesIn),
    [],
    "a size exists if and only if a build succeeded, so ask for the SIZE (Option/`null`) and handle \
its absence - never `!result.error`. An AGGREGATE may check its own `error`, but only if it also \
consults `incomplete` and `degraded`: a total can be short an input, or be a sum of the wrong \
quantity, with nothing having failed",
  );
});

test("no size-naming file uses an `error` early-exit as the gate in front of a size", () => {
  assert.deepEqual(
    sizeNamingFiles.flatMap(gateOffencesIn),
    [],
    "`if (result.error) { return }` in front of a size read is the same mistake as `!result.error`, \
written the other way round - and it is worse, because it also waves through the shape that has \
neither an error nor a size: a still-LOADING import. Ask for the size",
  );
});

/**
 * The corpus planted against this guard, and the exact number it catches.
 *
 * Eighteen of these twenty-four are detected. The six misses are named, with the reason, because a
 * guard whose true reach is unknown is a guard nobody can rely on — and an earlier version of this
 * file claimed "both polarities, both languages" while catching six of eighteen.
 *
 * **Adding a spelling here is how you find out what the guard is worth.** Plant it FIRST, mark
 * `detected` with what actually happens, and only then decide whether to widen a matcher. A corpus
 * assembled from the things a matcher already catches proves nothing at all.
 */
const BANNED_SPELLINGS = [
  // --- JS / TS ------------------------------------------------------------------------------
  {
    name: "canonical negation",
    language: "ts",
    detected: true,
    code: "if (!result.error) { render(result.brotliBytes); }",
  },
  {
    name: "negation, optional chain",
    language: "ts",
    detected: true,
    code: "const usable = !state.result?.error;",
  },
  {
    name: "negation through parens",
    language: "ts",
    detected: true,
    code: "const usable = !(result.error);",
  },
  {
    name: "negation in a filter",
    language: "ts",
    detected: true,
    code: "const sized = results.filter((item) => !item.error);",
  },
  {
    name: "negation split across lines",
    language: "ts",
    detected: true,
    code: "const usable =\n  !result\n    .error;",
  },
  {
    name: "negation hidden behind a block comment",
    language: "ts",
    detected: true,
    code: "/* the size */ if (!result.error) { render(result.brotliBytes); }",
  },
  {
    name: "inverted early-exit gate",
    language: "ts",
    detected: true,
    code: "if (result.error) {\n  return;\n}\nrender(result.brotliBytes);",
  },
  {
    name: "inverted gate, braceless",
    language: "ts",
    detected: true,
    code: "for (const item of items) {\n  if (item.error) continue;\n  total += item.brotliBytes;\n}",
  },
  {
    name: "inverted gate, compared to null",
    language: "ts",
    detected: true,
    code: "if (result.error !== null) {\n  return;\n}\nrender(result.brotliBytes);",
  },
  {
    name: "inverted gate, wrapped in Boolean()",
    language: "ts",
    detected: true,
    code: "if (Boolean(result.error)) {\n  return;\n}\nrender(result.brotliBytes);",
  },
  {
    name: "DESTRUCTURED error",
    language: "ts",
    detected: false,
    // The receiver is gone by the time the check is written, so there is no `.error` to match. A
    // parser would see it; a regex over text cannot.
    code: "const { error } = result;\nif (!error) { render(result.brotliBytes); }",
  },
  {
    name: "ternary on error",
    language: "ts",
    detected: false,
    // `error ? a : b` is a usability test with no negation and no early exit — and it is also how a
    // correct error REPORTER is written. Telling them apart needs types.
    code: 'const label = result.error ? "unmeasured" : formatBytes(result.brotliBytes);',
  },
  {
    name: "comparison as an expression",
    language: "ts",
    detected: false,
    // Indistinguishable, without types, from the nine wire-shape validators in `ipc/client.ts`
    // (`candidate.error === null || typeof candidate.error === "string"`).
    code: "const usable = result.error == null;",
  },
  // --- Rust ---------------------------------------------------------------------------------
  {
    name: "is_none",
    language: "rs",
    detected: true,
    code: "if result.error.is_none() { total += result.sizes().unwrap().brotli_bytes; }",
  },
  {
    name: "is_some, the other polarity",
    language: "rs",
    detected: true,
    code: "if result.error.is_some() { return None; }\nlet sizes = result.sizes();",
  },
  {
    name: "is_none behind as_ref",
    language: "rs",
    detected: true,
    code: "let sized = results.iter().filter(|item| item.error.as_ref().is_none());",
  },
  {
    name: "matches! against None",
    language: "rs",
    detected: true,
    code: "let usable = matches!(result.error, None);",
  },
  {
    name: "let-else on error",
    language: "rs",
    detected: true,
    code: "let Some(_) = result.error else {\n    return Some(result.sizes()?.brotli_bytes);\n};",
  },
  // --- Planted independently of the corpus above, which had been tuned against the matchers and so
  // --- flattered them: on THESE, the guard scored 3 of 13 before the Rust pattern-match and
  // --- combinator matchers were added. A corpus that only contains what you already catch measures
  // --- nothing. -----------------------------------------------------------------------------
  {
    name: "if-let on error",
    language: "rs",
    detected: true,
    code: "if let Some(_) = result.error { return None; }\nlet sizes = result.sizes();",
  },
  {
    name: "match on error",
    language: "rs",
    detected: true,
    code: "match result.error {\n    Some(_) => None,\n    None => result.sizes().map(|s| s.brotli_bytes),\n}",
  },
  {
    name: "map_or on error",
    language: "rs",
    detected: true,
    code: "let usable = result.error.map_or(true, |_| false);\nlet bytes = result.brotli_bytes;",
  },
  {
    name: "positive gate, === undefined",
    language: "ts",
    detected: false,
    // Not an early exit — the size read is INSIDE the guarded block, so `gateOffence`'s
    // exit-early discriminator does not fire. Identical in meaning to `if (!result.error)`.
    code: "if (result.error === undefined) {\n  render(result.brotliBytes);\n}",
  },
  {
    name: "=== undefined in a filter",
    language: "ts",
    detected: false,
    // A bare comparison in an expression: indistinguishable, without types, from the wire-shape
    // validators in `ipc/client.ts`. Same reason as "comparison as an expression".
    code: "const sized = imports.filter((i) => i.error === undefined);\nreturn sized.map((i) => i.brotliBytes);",
  },
  {
    name: "aliased error binding",
    language: "ts",
    detected: false,
    // The receiver is gone by the time the check is written, exactly as in "DESTRUCTURED error".
    code: "const failed = result.error;\nif (!failed) { render(result.brotliBytes); }",
  },
];

/**
 * What this guard catches, stated as a number so it cannot quietly drift from what it claims.
 *
 * Machine-computed against `BANNED_SPELLINGS` by the test below — not typed out by hand — so
 * widening or weakening any matcher moves this or fails.
 */
const STATED_COVERAGE = 18;

const detects = (spelling) =>
  gateOffencesIn({
    path: `corpus.${spelling.language}`,
    // Discovery is a separate concern from detection: every spelling here is planted in a file the
    // guard already scans, and one that names a size (the strictest population).
    text: `${spelling.code}\n`,
  }).length > 0;

test("the guard catches exactly the spellings it claims to", () => {
  const missed = BANNED_SPELLINGS.filter((spelling) => spelling.detected && !detects(spelling)).map(
    (spelling) => spelling.name,
  );
  assert.deepEqual(
    missed,
    [],
    "a spelling this guard CLAIMS to catch and does not - the matchers above have been weakened",
  );

  const caught = BANNED_SPELLINGS.filter(detects).length;
  assert.equal(
    caught,
    STATED_COVERAGE,
    `this guard catches ${caught} of the ${BANNED_SPELLINGS.length} planted spellings, and says it \
catches ${STATED_COVERAGE}. If you widened it, raise STATED_COVERAGE (and the header, and SRS \
FR-026c) to the number you actually reach. A guard that claims more coverage than it has is worse \
than no guard: it buys false confidence, which is how this defect survived seven rounds. The RULE is \
enforced at runtime, by the gate inside each durable store - not here`,
  );
});

test("no file defaults a missing size to zero", () => {
  const bannedShapes = [
    {
      name: "defaulting a missing size to zero (Rust)",
      // `brotli_bytes().unwrap_or_default()`, `.sizes().unwrap_or(...)`. Only the five MEASUREMENTS —
      // `shared_bytes.unwrap_or_default()` is fine, an absent shared count really is zero.
      pattern:
        /\b(?:raw|minified|gzip|brotli|zstd)_bytes(?:\(\))?\s*(?:\.\s*\w+\(\))*\s*\.\s*unwrap_or|\.sizes\(\)\s*(?:\.\s*\w+\(\))*\s*\.\s*unwrap_or/gu,
    },
    {
      name: "defaulting a missing size to zero (TypeScript)",
      // `result.brotli_bytes ?? 0`, `row.brotliBytes || 0`.
      pattern: /\b(?:raw|minified|gzip|brotli|zstd)(?:_bytes|Bytes)\s*(?:\?\?|\|\|)\s*0\b/gu,
    },
  ];

  const offences = [];

  for (const file of sizeNamingFiles) {
    const code = stripComments(file.text);
    for (const { name, pattern } of bannedShapes) {
      for (const match of code.matchAll(pattern)) {
        offences.push(
          `${file.path}:${lineAt(code, match.index)} (${name}): ${lineTextAt(file.text, match.index)}`,
        );
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
