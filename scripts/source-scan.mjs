import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Reading the repository's own source as text, for the Guards.
 *
 * Two guards scan the tree — the result-model guard (`result-model-guards.test.mjs`) and the
 * import-cost naming guard (`import-cost-naming-guards.test.mjs`) — and both need the same thing
 * first: **the code with the comments taken out.** A comment that can hide an offence silences a
 * guard, and a second copy of a lexer is a second thing to get wrong, so there is one.
 */

export const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

export const defaultSourceRoots = ["daemon/src", "extension/src", "cli", "scripts"];

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

/** Every source file under `roots`, as `{ path, text }` with a repo-relative POSIX path. */
export const sourceFiles = (roots = defaultSourceRoots) =>
  roots
    .flatMap((root) => walk(path.join(repoRoot, root)))
    .map((full) => ({
      path: path.relative(repoRoot, full).split(path.sep).join("/"),
      text: readFileSync(full, "utf8"),
    }));

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
export const endOfLiteral = (text, index) => {
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
export const endOfComment = (text, index) => {
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
 * A guard that skips a line *starting* with a comment token is silenced by a leading block comment:
 * `/* size *\/ if (!result.error) { … }` is invisible to it. Blanking in place cannot be dodged that
 * way. String and template literals are tracked (a URL is not a comment), as are Rust raw strings.
 */
export const stripComments = (text) => {
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

/** The 1-based line `index` falls on. */
export const lineAt = (text, index) => text.slice(0, index).split("\n").length;
