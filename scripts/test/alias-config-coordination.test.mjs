import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

// Drift check. The daemon reads the alias tables of the bundler configs it names in
// `daemon/src/pipeline/bundler_aliases.rs`, and only the extension's watcher can tell it one was
// edited. A config name the daemon reads and the extension does not watch keeps the old alias table
// until the daemon restarts; one the extension watches but does not classify as a config is sent as
// a package manifest. Rust and TypeScript cannot share the list, so the copies are held together here.

const repoFile = (relativePath) =>
  readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

const rustList = (source, name) => {
  const declaration = new RegExp(`const ${name}: \\[&str; \\d+\\] =\\s*\\[([^\\]]*)\\]`, "u").exec(
    source,
  );
  assert.ok(declaration, `the daemon must still declare ${name}`);
  return [...declaration[1].matchAll(/"([^"]+)"/gu)].map((match) => match[1]);
};

const daemonConfigNames = () => {
  const source = repoFile("daemon/src/pipeline/bundler_aliases.rs");
  const stems = rustList(source, "BUNDLER_CONFIG_STEMS");
  const extensions = rustList(source, "BUNDLER_CONFIG_EXTENSIONS");
  return stems.flatMap((stem) => extensions.map((extension) => `${stem}.${extension}`));
};

const extensionConfigName = () => {
  const declaration = /const workspaceConfigFileName =\s*\/(.+)\/(\w*);/u.exec(
    repoFile("extension/src/watcherInvalidation.ts"),
  );
  assert.ok(declaration, "the extension must still declare workspaceConfigFileName");
  return new RegExp(declaration[1], declaration[2]);
};

/** Expand the single `{a,b}` groups of a watcher glob's file-name part into every name it matches. */
const watchedNames = () => {
  const patterns = [...repoFile("extension/src/watcher.ts").matchAll(/"\*\*\/([^"*]+)"/gu)].map(
    (match) => match[1],
  );
  return patterns.flatMap((pattern) =>
    [...pattern.matchAll(/\{([^}]*)\}|[^{]+/gu)].reduce(
      (names, part) =>
        part[1] === undefined
          ? names.map((name) => name + part[0])
          : names.flatMap((name) => part[1].split(",").map((option) => name + option)),
      [""],
    ),
  );
};

test("the extension watches and classifies every bundler config the daemon reads", () => {
  const names = daemonConfigNames();
  assert.ok(names.length > 0, "the daemon must read at least one bundler config");

  const classifies = extensionConfigName();
  const watched = new Set(watchedNames());
  for (const name of names) {
    assert.ok(classifies.test(name), `${name} would be sent to the daemon as a package manifest`);
    assert.ok(watched.has(name), `an edit to ${name} would never reach the daemon`);
  }
});
