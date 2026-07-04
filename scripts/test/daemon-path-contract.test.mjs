import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { parseKnownHashesSource } from "../daemon-hashes.mjs";
import { daemonRoot, platformTargets, relativeDaemonPath } from "../targets.mjs";

// The daemon path is a runtime contract across three shipping boundaries that
// cannot share a module: the build scripts (targets.mjs), the bundled extension
// (platform.ts), and the standalone CLI (importlens.mjs). Each declares its own
// daemon root; these tests keep them in lockstep, so moving the binaries is a
// one-line change per boundary that fails loudly here when half-done.

const repoFile = (relativePath) => readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

test("extension platform.ts mirrors the scripts' daemon root", () => {
  const source = repoFile("extension/src/daemon/platform.ts");
  const match = /export const daemonRoot = "([^"]+)";/u.exec(source);

  assert.ok(match, "platform.ts must declare `export const daemonRoot = \"...\";`");
  assert.equal(match[1], daemonRoot);
});

test("CLI importlens.mjs mirrors the scripts' daemon root", () => {
  const source = repoFile("cli/importlens.mjs");
  const match = /const DAEMON_ROOT = "([^"]+)";/u.exec(source);

  assert.ok(match, "importlens.mjs must declare `const DAEMON_ROOT = \"...\";`");
  assert.equal(match[1], daemonRoot);
});

test("nativeTransport derives the daemon path from platform.ts, not inline", () => {
  const source = repoFile("extension/src/daemon/nativeTransport.ts");

  assert.match(source, /daemonRelativePath\(target\)/u);
  assert.doesNotMatch(source, /`bin\//u);
});

test("every committed daemon hash key is a canonical daemon path", () => {
  // A stale or half-renamed key set would make integrity verification fail at
  // runtime and silently disable the daemon on the affected platforms.
  const source = repoFile("extension/src/daemon/knownHashes.generated.ts");
  const keys = Object.keys(parseKnownHashesSource(source));
  const canonical = new Set(platformTargets.map((target) => relativeDaemonPath(target)));

  assert.ok(keys.length > 0, "knownHashes.generated.ts must contain at least one entry");
  for (const key of keys) {
    assert.ok(canonical.has(key), `stale daemon hash key ${key}; rename it to match relativeDaemonPath()`);
  }
});
