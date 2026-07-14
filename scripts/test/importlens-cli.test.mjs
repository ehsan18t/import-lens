import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { encode } from "@msgpack/msgpack";
import {
  analyzeFileWithDaemon,
  createDaemonClient,
  daemonBinaryPath,
  EXIT_BUDGET_EXCEEDED,
  EXIT_COULD_NOT_MEASURE,
  isUsableFileSize,
  loadBudgetConfig,
  parseCliArgs,
  resolveCliStoragePaths,
  runImportLensCheck,
} from "../../cli/importlens.mjs";

const analyzed = (overrides = {}) => ({
  filePath: "/workspace/src/app.ts",
  brotliBytes: 1200,
  error: null,
  incomplete: false,
  degraded: false,
  diagnostics: [],
  unmeasured: [],
  imports: [{ specifier: "small-lib", brotliBytes: 900 }],
  ...overrides,
});

class FakeSocket extends EventEmitter {
  writes = [];

  write(frame) {
    this.writes.push(frame);
    return true;
  }
}

const frame = (message) => {
  const payload = Buffer.from(encode(message));
  const header = Buffer.allocUnsafe(4);
  header.writeUInt32BE(payload.length, 0);
  return Buffer.concat([header, payload]);
};

test("parseCliArgs supports importlens check and optional config", () => {
  assert.deepEqual(parseCliArgs(["check"]), { command: "check", configPath: undefined });
  assert.deepEqual(parseCliArgs(["check", "--config", ".importlensrc.json"]), {
    command: "check",
    configPath: ".importlensrc.json",
  });
  assert.throws(() => parseCliArgs([]), /Usage:/u);
});

test("loadBudgetConfig rejects malformed budget config", async () => {
  await assert.rejects(
    () =>
      loadBudgetConfig({
        configPath: ".importlensrc.json",
        readText: async () => "{ invalid json",
        findDefaultConfig: async () => null,
      }),
    /failed to parse/u,
  );
});

test("runImportLensCheck exits non-zero on daemon-backed budget violations", async () => {
  const output = [];
  const exitCode = await runImportLensCheck({
    cwd: "/workspace",
    budgets: { perImportBrotliBytes: 1000, perFileBrotliBytes: 2000 },
    changedFiles: async () => ["src/app.ts"],
    analyzeFile: async () =>
      analyzed({
        brotliBytes: 2500,
        imports: [
          { specifier: "large-lib", brotliBytes: 1500 },
          { specifier: "small-lib", brotliBytes: 500 },
        ],
      }),
    writeLine: (line) => output.push(line),
  });

  assert.equal(exitCode, EXIT_BUDGET_EXCEEDED);
  assert.deepEqual(output, [
    "src/app.ts: file Brotli budget exceeded: 2.5 kB > 2.0 kB",
    "src/app.ts: large-lib Brotli budget exceeded: 1.5 kB > 1.0 kB",
  ]);
});

test("runImportLensCheck passes when changed files are within budgets", async () => {
  const output = [];
  const exitCode = await runImportLensCheck({
    cwd: "/workspace",
    budgets: { perImportBrotliBytes: 2000, perFileBrotliBytes: 3000 },
    changedFiles: async () => ["src/app.ts"],
    analyzeFile: async () => analyzed(),
    writeLine: (line) => output.push(line),
  });

  assert.equal(exitCode, 0);
  assert.deepEqual(output, ["Import Lens budgets passed for 1 changed file."]);
});

// Defect #6, and the worst of the six: an import whose build timed out used to reach this gate
// with `error: null` and a fabricated size, or vanish from the file total altogether — and CI went
// GREEN, so the regression merged. A gate that cannot measure must never report success
// (ADR-0006, invariant 5).
test("runImportLensCheck refuses a verdict when an import could not be measured", async () => {
  const output = [];
  const exitCode = await runImportLensCheck({
    cwd: "/workspace",
    budgets: { perImportBrotliBytes: 2000, perFileBrotliBytes: 3000 },
    changedFiles: async () => ["src/app.ts"],
    analyzeFile: async () =>
      analyzed({
        // Everything the gate can see says PASS: the file total is under budget, and every import
        // it has a size for is under budget too. The only evidence is the missing import.
        incomplete: true,
        unmeasured: [{ specifier: "lodash-es", stage: "timeout", transient: true }],
      }),
    writeLine: (line) => output.push(line),
  });

  assert.equal(
    exitCode,
    EXIT_COULD_NOT_MEASURE,
    "a flaky CI box must be diagnosable, and must never be confused with a pass OR a regression",
  );
  assert.notEqual(EXIT_COULD_NOT_MEASURE, EXIT_BUDGET_EXCEEDED);
  assert.deepEqual(output, [
    "src/app.ts: an import that belongs in this file's total was not measured, so the number is a floor and not the file's size (1 unmeasured import; stage: timeout) - budget not evaluated",
    "Import Lens could not measure every changed file; those files' budgets were NOT evaluated. This is not a regression. A transient stage (timeout/panic/engine_gone) may pass on a re-run; any other cause is a package this build cannot measure, and it will not.",
  ]);
});

/**
 * The file's OWN total is sound: its combined build succeeded, so nothing is missing from it — a
 * combined build owes nothing to the per-import builds beside it (ADR-0006, invariant 4). What
 * failed is one import's separate build, so it is that IMPORT's budget that was not evaluated.
 *
 * The line used to say "file total is a floor", which is a claim about a number that is not a floor.
 * The verdict (exit 3) is unchanged and correct: a gate that could not measure something it was
 * asked to judge must never report success.
 */
test("a transient import failure does not make a sound file total a floor", async () => {
  const output = [];
  const exitCode = await runImportLensCheck({
    cwd: "/workspace",
    budgets: { perImportBrotliBytes: 2000, perFileBrotliBytes: 3000 },
    changedFiles: async () => ["src/app.ts"],
    analyzeFile: async () =>
      analyzed({
        // The combined build landed. One import's own build timed out.
        incomplete: false,
        unmeasured: [{ specifier: "lodash-es", stage: "timeout", transient: true }],
      }),
    writeLine: (line) => output.push(line),
  });

  assert.equal(exitCode, EXIT_COULD_NOT_MEASURE);
  assert.deepEqual(output, [
    "src/app.ts: could not measure 1 import (stage: timeout) - budgets not evaluated",
    "Import Lens could not measure every changed file; those files' budgets were NOT evaluated. This is not a regression. A transient stage (timeout/panic/engine_gone) may pass on a re-run; any other cause is a package this build cannot measure, and it will not.",
  ]);
});

// **The seventh instance, at the CI gate.** A DETERMINISTIC failure has no size, so it contributes
// nothing to its file's total — and the daemon now says so (`incomplete`, FR-024a), because the same
// failure also kills the file's combined build and the sum that survives is not the file's size.
// This gate used to be handed `incomplete: false` for exactly that file and reported PASS.
//
// Deterministically-unknown is still unknown. The verdict is "not evaluated" (exit 3), and — the
// second half — a real violation found in a file that WAS measured is still printed, instead of
// being swallowed by the exit code that outranks it.
test("runImportLensCheck refuses a verdict for a deterministically unmeasurable file", async () => {
  const output = [];
  const exitCode = await runImportLensCheck({
    cwd: "/workspace",
    budgets: { perImportBrotliBytes: 1000 },
    changedFiles: async () => ["src/app.ts", "src/measured.ts"],
    analyzeFile: async (filePath) =>
      filePath.endsWith("app.ts")
        ? analyzed({
            filePath,
            // What the daemon now sends for this file: a parse failure means one import's bytes are
            // missing from the total, so the total is a floor.
            incomplete: true,
            unmeasured: [{ specifier: "broken-lib", stage: "parse", transient: false }],
          })
        : analyzed({ filePath, imports: [{ specifier: "large-lib", brotliBytes: 1500 }] }),
    writeLine: (line) => output.push(line),
  });

  assert.equal(
    exitCode,
    EXIT_COULD_NOT_MEASURE,
    "a gate that cannot measure must never report success - not even when the reason is permanent",
  );
  assert.deepEqual(output, [
    "src/measured.ts: large-lib Brotli budget exceeded: 1.5 kB > 1.0 kB",
    "src/app.ts: an import that belongs in this file's total was not measured, so the number is a floor and not the file's size - budget not evaluated",
    "Import Lens could not measure every changed file; those files' budgets were NOT evaluated. This is not a regression. A transient stage (timeout/panic/engine_gone) may pass on a re-run; any other cause is a package this build cannot measure, and it will not.",
  ]);
});

// **The eighth instance, and the one the seventh fix left standing.** A file imports two packages.
// Both are individually Measured and cached. The file's own COMBINED build — strictly larger than
// either, and so the likeliest thing in the system to hit the build timeout — fails. The daemon
// falls back to a sum of the per-import costs, and every contributor being Measured leaves
// `incomplete: false`. The wire response is then: `incomplete: false`, `error: null`, a size on
// every import, and `brotli_bytes` holding an UN-DEDUPLICATED sum — a Combined Import Cost, which
// ADR-0004 says is a different quantity from a File Cost, and an over-count of it.
//
// `FileSizeCache::insert` refuses that number. `isDurableFileSize` refuses it. This gate read it and
// issued a pass/fail verdict, which ADR-0006 invariant 3 calls a durable store like any other.
//
// An over-count cannot produce a false PASS — but it can produce a false FAIL, and invariant 5
// forbids both: a budget judged against a number the file never had is neither passed nor failed.
test("runImportLensCheck refuses a verdict when the file's own combined build failed", async () => {
  const output = [];
  const exitCode = await runImportLensCheck({
    cwd: "/workspace",
    budgets: { perFileBrotliBytes: 1000 },
    changedFiles: async () => ["src/app.ts"],
    analyzeFile: async () =>
      analyzed({
        // Every import measured. Nothing unmeasured. No error. The only thing wrong with this
        // number is that it is not this file's.
        degraded: true,
        brotliBytes: 2500,
        unmeasured: [],
        imports: [
          { specifier: "alpha", brotliBytes: 1500 },
          { specifier: "beta", brotliBytes: 1000 },
        ],
      }),
    writeLine: (line) => output.push(line),
  });

  assert.equal(
    exitCode,
    EXIT_COULD_NOT_MEASURE,
    "2500 > 1000 looks like a regression, and it is not one - it is a sum of the wrong quantity",
  );
  assert.deepEqual(output, [
    "src/app.ts: the file's combined build failed, so the number is an un-deduplicated sum of its imports and not the file's size - budget not evaluated",
    "Import Lens could not measure every changed file; those files' budgets were NOT evaluated. This is not a regression. A transient stage (timeout/panic/engine_gone) may pass on a re-run; any other cause is a package this build cannot measure, and it will not.",
  ]);
});

// The canonical wire shape of the eighth instance: the combined build did not merely fail, it TIMED
// OUT — the biggest build in the system hitting the daemon's deadline. The daemon then sets `degraded`
// AND pushes the `timeout` stage into the aggregate's own diagnostics, while every import stays
// Measured (`incomplete: false`). This is the shape the previous test skipped, with an empty
// `diagnostics`. The transient stage on the aggregate must NOT add "and an import that belongs in it
// was not measured either" to the line — that clause claims a missing contributor a timed-out combined
// build does not have. The timeout is the combined build's OWN failure, which `degraded` already says.
test("runImportLensCheck names a timed-out combined build a Combined Import Cost, without a false missing-import clause", async () => {
  const output = [];
  const exitCode = await runImportLensCheck({
    cwd: "/workspace",
    budgets: { perFileBrotliBytes: 1000 },
    changedFiles: async () => ["src/app.ts"],
    analyzeFile: async () =>
      analyzed({
        // The combined build timed out. `degraded`, and its `timeout` stage on the aggregate's own
        // diagnostics — but every import is Measured, so nothing is actually missing from the sum.
        degraded: true,
        brotliBytes: 2500,
        diagnostics: [{ stage: "timeout", message: "build cancelled", details: [] }],
        unmeasured: [],
        imports: [
          { specifier: "alpha", brotliBytes: 1500 },
          { specifier: "beta", brotliBytes: 1000 },
        ],
      }),
    writeLine: (line) => output.push(line),
  });

  assert.equal(exitCode, EXIT_COULD_NOT_MEASURE);
  assert.deepEqual(output, [
    "src/app.ts: the file's combined build failed, so the number is an un-deduplicated sum of its imports and not the file's size - budget not evaluated",
    "Import Lens could not measure every changed file; those files' budgets were NOT evaluated. This is not a regression. A transient stage (timeout/panic/engine_gone) may pass on a re-run; any other cause is a package this build cannot measure, and it will not.",
  ]);
});

// One file whose AGGREGATE failed outright (`error: Some` — no bytes at all) used to throw out of
// `analyzeFileWithDaemon`, which `main` catches into exit 2 — abandoning the whole run, and taking
// every other changed file's budget with it. Exit 2 also means "the CLI broke", not the exit 3
// FR-032a mandates for "could not measure", so CI could not tell one from the other.
test("analyzeFileWithDaemon reports a failed aggregate instead of aborting the run", async () => {
  const workspace = mkdtempSync(path.join(tmpdir(), "importlens-cli-"));
  const brokenPath = path.join(workspace, "broken.ts");
  writeFileSync(brokenPath, "import 'broken-lib';\n");

  const daemon = {
    request: async () => ({
      request_id: 1,
      raw_bytes: 0,
      brotli_bytes: 0,
      imports: [],
      states: [],
      incomplete: false,
      degraded: false,
      error: "no import could be sized conservatively",
      diagnostics: [],
    }),
  };

  try {
    const result = await analyzeFileWithDaemon(brokenPath, workspace, daemon);

    assert.equal(result.error, "no import could be sized conservatively");
    assert.equal(isUsableFileSize(result), false, "and the gate refuses it, as it always did");

    // End to end: the failed file is reported, the OTHER file is still analyzed and its violation
    // still printed, and the verdict is exit 3 - never the exit 2 of an aborted run.
    const output = [];
    const analyzedPaths = [];
    const exitCode = await runImportLensCheck({
      cwd: workspace,
      budgets: { perImportBrotliBytes: 1000 },
      changedFiles: async () => ["broken.ts", "measured.ts"],
      analyzeFile: async (filePath) => {
        analyzedPaths.push(filePath);
        return filePath.endsWith("broken.ts")
          ? analyzeFileWithDaemon(filePath, workspace, daemon)
          : {
              filePath,
              brotliBytes: 900,
              error: null,
              incomplete: false,
              degraded: false,
              diagnostics: [],
              unmeasured: [],
              imports: [{ specifier: "large-lib", brotliBytes: 1500 }],
            };
      },
      writeLine: (line) => output.push(line),
    });

    assert.equal(
      analyzedPaths.length,
      2,
      "one unmeasurable file must not abandon the rest of the run",
    );
    assert.equal(exitCode, EXIT_COULD_NOT_MEASURE);
    assert.deepEqual(output, [
      "measured.ts: large-lib Brotli budget exceeded: 1.5 kB > 1.0 kB",
      "broken.ts: could not measure this file (no import could be sized conservatively) - budget not evaluated",
      "Import Lens could not measure every changed file; those files' budgets were NOT evaluated. This is not a regression. A transient stage (timeout/panic/engine_gone) may pass on a re-run; any other cause is a package this build cannot measure, and it will not.",
    ]);
  } finally {
    rmSync(workspace, { recursive: true, force: true });
  }
});

// The gate the CLI applies to the raw wire response, in isolation. The daemon's
// `FileSizeComputation::is_cacheable` and the extension's `isDurableFileSize` are the same rule, and
// a drift check holds all three together (file-size-usability-coordination.test.mjs).
test("isUsableFileSize refuses every shape that is not this file's size", () => {
  const response = (overrides = {}) => ({
    error: null,
    incomplete: false,
    degraded: false,
    diagnostics: [],
    ...overrides,
  });

  assert.equal(isUsableFileSize(response()), true, "a clean measurement IS judged");
  assert.equal(isUsableFileSize(response({ error: "no import could be sized" })), false);
  assert.equal(isUsableFileSize(response({ incomplete: true })), false, "a floor: an under-count");
  assert.equal(
    isUsableFileSize(response({ degraded: true })),
    false,
    "a per-import sum: an OVER-count, and the one with no other signal on the wire",
  );
  assert.equal(
    isUsableFileSize(response({ diagnostics: [{ stage: "timeout", message: "x", details: [] }] })),
    false,
    "a transient stage on the aggregate's own diagnostics",
  );
  // A DETERMINISTIC stage on the aggregate is not, by itself, a reason to refuse: a `types_only`
  // diagnostic rides on a perfectly complete total. `degraded` is what says the build failed.
  assert.equal(
    isUsableFileSize(
      response({ diagnostics: [{ stage: "types_only", message: "x", details: [] }] }),
    ),
    true,
    "without this, the fix could be 'made to pass' by refusing every total that carries a diagnostic",
  );
});

test("runImportLensCheck resolves changed files against resolveRoot but reports relative to cwd", async () => {
  const repoRoot = path.resolve("/repo");
  const packageDir = path.join(repoRoot, "packages", "app");
  const analyzedPaths = [];
  const output = [];

  const exitCode = await runImportLensCheck({
    cwd: packageDir,
    resolveRoot: repoRoot,
    budgets: { perFileBrotliBytes: 1000 },
    // git diff prints repository-root-relative paths regardless of cwd.
    changedFiles: async () => ["packages/app/src/index.ts"],
    analyzeFile: async (filePath) => {
      analyzedPaths.push(filePath);
      return analyzed({ filePath, brotliBytes: 2500, imports: [] });
    },
    writeLine: (line) => output.push(line),
  });

  assert.deepEqual(analyzedPaths, [path.join(repoRoot, "packages", "app", "src", "index.ts")]);
  assert.equal(exitCode, 1);
  assert.deepEqual(output, ["src/index.ts: file Brotli budget exceeded: 2.5 kB > 1.0 kB"]);
});

test("daemonBinaryPath resolves from the installed package root", () => {
  assert.equal(
    daemonBinaryPath({ packageRoot: path.join("C:", "ImportLens"), platformTarget: "win32-x64" }),
    path.join("C:", "ImportLens", "dist", "bin", "win32-x64", "import-lens-daemon.exe"),
  );
});

test("resolveCliStoragePaths keeps daemon cache outside the project directory", () => {
  const cwd = path.join("C:", "workspace", "app");
  const paths = resolveCliStoragePaths({
    env: { LOCALAPPDATA: path.join("C:", "Users", "Ehsan", "AppData", "Local") },
    platform: "win32",
    homeDir: path.join("C:", "Users", "Ehsan"),
  });

  assert.equal(
    paths.cachePath,
    path.join("C:", "Users", "Ehsan", "AppData", "Local", "ImportLens", "daemon-cache"),
  );
  assert.equal(
    paths.lifecyclePath,
    path.join("C:", "Users", "Ehsan", "AppData", "Local", "ImportLens", "daemon-lifecycle"),
  );
  assert.equal(paths.cachePath.startsWith(cwd), false);
});

test("createDaemonClient resolves concurrent responses by request id", async () => {
  const socket = new FakeSocket();
  const client = createDaemonClient(socket);
  const first = client.request({ type: "file_size", request_id: 1 }, 100);
  const second = client.request({ type: "file_size", request_id: 2 }, 100);

  socket.emit("data", frame({ request_id: 2, ok: "second" }));
  socket.emit("data", frame({ request_id: 1, ok: "first" }));

  assert.deepEqual(await Promise.all([first, second]), [
    { request_id: 1, ok: "first" },
    { request_id: 2, ok: "second" },
  ]);
});

test("createDaemonClient rejects pending requests on timeout and close", async () => {
  const timeoutSocket = new FakeSocket();
  const timeoutClient = createDaemonClient(timeoutSocket);

  await assert.rejects(
    () => timeoutClient.request({ type: "file_size", request_id: 3 }, 1),
    /timed out/u,
  );

  const closeSocket = new FakeSocket();
  const closeClient = createDaemonClient(closeSocket);
  const pending = closeClient.request({ type: "file_size", request_id: 4 }, 100);
  closeSocket.emit("close");

  await assert.rejects(pending, /IPC socket closed/u);
});

test("createDaemonClient rejects pending requests on malformed frames", async () => {
  const socket = new FakeSocket();
  const client = createDaemonClient(socket);
  const pending = client.request({ type: "file_size", request_id: 5 }, 100);
  const invalidPayload = Buffer.from([0xc1]);
  const header = Buffer.allocUnsafe(4);
  header.writeUInt32BE(invalidPayload.length, 0);

  socket.emit("data", Buffer.concat([header, invalidPayload]));

  await assert.rejects(pending);
});
