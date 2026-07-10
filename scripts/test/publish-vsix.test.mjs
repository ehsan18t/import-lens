import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";

import {
  backoffMs,
  formatSummary,
  isRetryable,
  maxAttempts,
  outcomeFor,
  publishAll,
  publisherArgv,
  publishOne,
  targetForVsix,
} from "../publish-vsix.mjs";

const publisher = { patEnv: "FAKE_PAT", argv: () => [] };

// Feeds publishOne a canned response per attempt and records the backoff waits.
const stub = (...responses) => {
  const waits = [];
  let attempt = 0;

  return {
    waits,
    deps: {
      run: () => responses[Math.min(attempt++, responses.length - 1)],
      sleep: async (ms) => {
        waits.push(ms);
      },
    },
  };
};

const transient = { ok: false, output: "ERROR  Failed Request: Bad Gateway(502)" };
const terminal = { ok: false, output: "ERROR  Failed Request: Unauthorized(401)" };
const ok = { ok: true, output: " DONE  Published publisher.import-lens v1.2.3." };

test("both stores are invoked with --skip-duplicate and no token in argv", () => {
  const vsce = publisherArgv("vsce", "dist/vsix/import-lens-linux-arm64-1.2.3.vsix");
  const ovsx = publisherArgv("ovsx", "dist/vsix/import-lens-linux-arm64-1.2.3.vsix");

  assert.deepEqual(vsce, [
    "exec",
    "vsce",
    "publish",
    "--packagePath",
    "dist/vsix/import-lens-linux-arm64-1.2.3.vsix",
    "--skip-duplicate",
  ]);
  assert.deepEqual(ovsx, [
    "exec",
    "ovsx",
    "publish",
    "dist/vsix/import-lens-linux-arm64-1.2.3.vsix",
    "--skip-duplicate",
  ]);

  for (const argv of [vsce, ovsx]) {
    assert.ok(!argv.includes("--pat"), "the PAT must come from the environment, not argv");
  }

  assert.equal(publisherArgv("unknown", "x.vsix"), undefined);
});

test("transient store failures retry and terminal ones do not", () => {
  // vsce prints the error message alone: either `Failed request: (503)` from
  // typed-rest-client when the body is empty, or the raw response body.
  assert.ok(isRetryable("ERROR  Failed request: (429)"));
  assert.ok(isRetryable("ERROR  Failed request: (500)"));
  assert.ok(isRetryable("ERROR  Failed request: (503)"));
  assert.ok(isRetryable("ERROR  Too Many Requests"));

  // ovsx never parenthesises the code; it names the status instead.
  assert.ok(isRetryable("The server responded with status 502: Bad Gateway"));
  assert.ok(isRetryable("The server responded with status 503."));
  assert.ok(isRetryable("The server responded with status 429: Too Many Requests"));

  assert.ok(isRetryable("statusCode: 503"));
  assert.ok(isRetryable("Error: socket hang up"));
  assert.ok(isRetryable("read ECONNRESET"));

  // A bad token or a rejected package will fail identically on every attempt.
  assert.ok(!isRetryable("ERROR  Unauthorized(401)"));
  assert.ok(!isRetryable("The server responded with status 401: Unauthorized"));
  assert.ok(!isRetryable("The server responded with status 403: Forbidden"));
  assert.ok(!isRetryable("ERROR  Extension entrypoint is missing."));
  assert.ok(!isRetryable("ERROR  publisher.import-lens (darwin-arm64) v1.2.3 already exists."));
  // Observed for real when a VSIX path is wrong; ENOENT must not look like ENOTFOUND.
  assert.ok(!isRetryable("ERROR  ENOENT: no such file or directory, open 'a.vsix'"));
});

test("backoff grows per attempt and stays bounded", () => {
  assert.equal(backoffMs(1), 5_000);
  assert.equal(backoffMs(2), 10_000);
  assert.equal(backoffMs(3), 20_000);
  assert.equal(backoffMs(10), 30_000);
  assert.ok(maxAttempts >= 2);
});

test("an already-published target reads as skipped rather than published", () => {
  assert.equal(outcomeFor("Version 1.2.3 is already published. Skipping publish."), "skipped");
  assert.equal(
    outcomeFor("publisher.import-lens v1.2.3 is already published. Skipping publish."),
    "skipped",
  );
  assert.equal(outcomeFor(" DONE  Published publisher.import-lens v1.2.3."), "published");
});

test("the platform target is recovered from the VSIX filename", () => {
  assert.equal(targetForVsix("dist/vsix/import-lens-darwin-arm64-1.2.3.vsix"), "darwin-arm64");
  assert.equal(targetForVsix("dist/vsix/import-lens-win32-x64-1.2.3.vsix"), "win32-x64");
  // linux-x64 must not be shadowed by the linux-arm64 entry, or vice versa.
  assert.equal(targetForVsix("dist/vsix/import-lens-linux-x64-1.2.3.vsix"), "linux-x64");
  assert.equal(targetForVsix("dist/vsix/unrecognised.vsix"), "unrecognised.vsix");
});

test("a transient failure is retried with backoff and can still succeed", async () => {
  const { waits, deps } = stub(transient, ok);

  const result = await publishOne(publisher, "dist/vsix/import-lens-linux-arm64-1.2.3.vsix", deps);

  assert.equal(result.outcome, "published");
  assert.equal(result.attempts, 2);
  assert.deepEqual(waits, [5_000]);
});

test("a terminal failure is not retried", async () => {
  const { waits, deps } = stub(terminal);

  const result = await publishOne(publisher, "dist/vsix/import-lens-linux-arm64-1.2.3.vsix", deps);

  assert.equal(result.outcome, "failed");
  assert.equal(result.attempts, 1);
  assert.deepEqual(waits, [], "a bad token fails identically on every attempt");
});

test("a target that stays transient gives up after maxAttempts", async () => {
  const { waits, deps } = stub(transient);

  const result = await publishOne(publisher, "dist/vsix/import-lens-linux-arm64-1.2.3.vsix", deps);

  assert.equal(result.outcome, "failed");
  assert.equal(result.attempts, maxAttempts);
  assert.deepEqual(waits, [5_000, 10_000]);
  assert.match(result.output, /Bad Gateway/u);
});

test("one failing target does not strand the targets behind it", async () => {
  // The original bug: `set -e` over a sorted glob aborted at linux-arm64, so
  // the two win32 packages were never even attempted.
  const failing = "import-lens-linux-arm64-1.2.3.vsix";
  const deps = {
    run: (_publisher, file) => (path.basename(file) === failing ? terminal : ok),
    sleep: async () => {},
  };

  const results = await publishAll(
    publisher,
    [
      "dist/vsix/import-lens-darwin-arm64-1.2.3.vsix",
      `dist/vsix/${failing}`,
      "dist/vsix/import-lens-win32-x64-1.2.3.vsix",
    ],
    deps,
  );

  assert.deepEqual(
    results.map(({ target, outcome }) => [target, outcome]),
    [
      ["darwin-arm64", "published"],
      ["linux-arm64", "failed"],
      ["win32-x64", "published"],
    ],
  );
});

test("the summary reports every target so a partial publish is legible", () => {
  const summary = formatSummary([
    { target: "darwin-arm64", outcome: "skipped", attempts: 1 },
    { target: "linux-arm64", outcome: "published", attempts: 2 },
    { target: "win32-x64", outcome: "failed", attempts: 3 },
  ]);

  assert.match(summary, /\| darwin-arm64 \| skipped \| 1 \|/u);
  assert.match(summary, /\| linux-arm64 \| published \| 2 \|/u);
  assert.match(summary, /\| win32-x64 \| failed \| 3 \|/u);
});
