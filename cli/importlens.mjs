#!/usr/bin/env node
import { execFile as execFileCallback, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdir, readFile } from "node:fs/promises";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { decode, encode } from "@msgpack/msgpack";

const execFile = promisify(execFileCallback);
const protocolVersion = 6;
const supportedExtensions = new Set([
  ".js",
  ".jsx",
  ".ts",
  ".tsx",
  ".mjs",
  ".mts",
  ".cjs",
  ".cts",
  ".svelte",
  ".vue",
  ".astro",
]);
const defaultIpcTimeoutMs = 10000;

export const parseCliArgs = (argv) => {
  const [command, ...rest] = argv;

  if (command !== "check") {
    throw new Error(usage());
  }

  let configPath;
  for (let index = 0; index < rest.length; index += 1) {
    const arg = rest[index];

    if (arg === "--config") {
      configPath = rest[index + 1];
      if (!configPath) {
        throw new Error("--config requires a path");
      }
      index += 1;
      continue;
    }

    throw new Error(`Unknown option: ${arg}\n${usage()}`);
  }

  return { command, configPath };
};

export const loadBudgetConfig = async ({
  configPath,
  readText = (filePath) => readFile(filePath, "utf8"),
  findDefaultConfig = findDefaultBudgetConfig,
} = {}) => {
  const source = configPath
    ? { path: configPath, text: await readText(configPath) }
    : await findDefaultConfig(readText);

  if (!source) {
    return {};
  }

  let parsed;
  try {
    parsed = JSON.parse(source.text);
  } catch (error) {
    throw new Error(
      `failed to parse ${source.path}: ${error instanceof Error ? error.message : String(error)}`,
    );
  }

  return sanitizeBudgets(parsed.budgets ?? parsed.importLens?.budgets ?? {});
};

// A budget was exceeded: the regression is real, and this is the code CI is meant to fail on.
export const EXIT_BUDGET_EXCEEDED = 1;
// The gate could not measure what it was asked to judge. DISTINCT from a budget failure on
// purpose: a flaky agent (a build that timed out, unwound, or lost the engine) says nothing about
// the code, and must never be reported as a regression — nor, which is far worse, as a PASS.
//
// The old gate did exactly that. It filtered imports with `!item.error` — the negative check —
// read `response.brotli_bytes` while discarding `incomplete`, and so a transient failure simply
// dropped the import from the comparison and CI went green. The regression merged. That is the
// sixth and worst instance of the one defect ADR-0006 exists to end, and it is why "cannot
// measure" is now an outcome of its own rather than an absence of violations.
export const EXIT_COULD_NOT_MEASURE = 3;

export const runImportLensCheck = async ({
  cwd = process.cwd(),
  resolveRoot = cwd,
  budgets,
  changedFiles,
  analyzeFile,
  writeLine = (line) => process.stdout.write(`${line}\n`),
}) => {
  if (!hasBudgets(budgets)) {
    writeLine("No Import Lens budgets configured.");
    return 0;
  }

  const files = (await changedFiles())
    .filter((filePath) => supportedExtensions.has(path.extname(filePath)))
    .sort();
  const violations = [];
  const unmeasurable = [];

  for (const filePath of files) {
    const result = await analyzeFile(path.resolve(resolveRoot, filePath));
    const relative = path.relative(cwd, result.filePath).split(path.sep).join("/");
    // A floor cannot absolve a budget and must not be allowed to: `incomplete` says an import that
    // belongs in this file's total contributed no bytes, so the real number is larger than the one
    // in hand by an unknown amount, and "under budget" is not a fact this run established. It is set
    // by ANY unmeasured contributor now, deterministic ones included — a package this build cannot
    // measure leaves the file's size just as unknown as one that timed out, and CI is the one place
    // where nobody is looking at the screen to notice. No verdict from a floor (ADR-0006,
    // invariant 5).
    //
    // And a DEGRADED total cannot condemn one. `degraded` says the file's own combined build failed
    // and the number fell back to a sum of per-import costs — a Combined Import Cost, which ADR-0004
    // calls a different quantity from a File Cost, because a module two imports share is counted
    // twice. It is an OVER-count, so it cannot pass a budget it should have failed... but it can
    // FAIL one it should have passed, and invariant 5 forbids both: a budget judged against a number
    // the file never had is neither passed nor failed. The daemon refuses to cache this shape and
    // the extension refuses to persist it; this gate is the third consumer of the same rule, and it
    // is the one that was still issuing a verdict.
    const transient = (result.unmeasured ?? []).filter((item) => item.transient);

    // **The gate is HERE, in the thing that issues the verdict** — not in `analyzeFile`, which is
    // injected and could be replaced by a caller who forgot it. A pass/fail verdict is a durable
    // store (ADR-0006, invariant 3), and a store that trusts its caller to have asked is a store
    // that eventually will not be asked. The daemon and the extension both learned this the same
    // way, and moved their gates inside their stores.
    if (!isUsableFileSize(result) || transient.length > 0) {
      unmeasurable.push({
        relative,
        transient,
        degraded: result.degraded === true,
        error: result.error ?? null,
      });
      continue;
    }

    if (
      budgets.perFileBrotliBytes !== undefined &&
      result.brotliBytes > budgets.perFileBrotliBytes
    ) {
      violations.push(
        `${relative}: file Brotli budget exceeded: ${formatBytes(result.brotliBytes)} > ${formatBytes(budgets.perFileBrotliBytes)}`,
      );
    }

    for (const item of result.imports) {
      if (
        budgets.perImportBrotliBytes !== undefined &&
        item.brotliBytes > budgets.perImportBrotliBytes
      ) {
        violations.push(
          `${relative}: ${item.specifier} Brotli budget exceeded: ${formatBytes(item.brotliBytes)} > ${formatBytes(budgets.perImportBrotliBytes)}`,
        );
      }
    }
  }

  // A confirmed violation in a file that WAS measured is printed either way. It used to be
  // swallowed whenever any other file was unmeasurable — the exit code won, and the finding went
  // with it — which now matters far more than it did, because a deterministically-unmeasurable
  // import makes its file a floor (FR-024a), so exit 3 is no longer rare. The exit CODE still gives
  // precedence to "could not measure"; what is not hidden is what was found.
  for (const violation of violations) {
    writeLine(violation);
  }

  // "Could not measure" wins the verdict: a run that could not measure some of what it was asked to
  // judge has not evaluated those budgets, whatever the others did.
  if (unmeasurable.length > 0) {
    for (const item of unmeasurable) {
      writeLine(unmeasurableLine(item));
    }
    writeLine(
      "Import Lens could not measure every changed file; those files' budgets were NOT evaluated. This is not a regression. A transient stage (timeout/panic/engine_gone) may pass on a re-run; any other cause is a package this build cannot measure, and it will not.",
    );
    return EXIT_COULD_NOT_MEASURE;
  }

  if (violations.length > 0) {
    return EXIT_BUDGET_EXCEEDED;
  }

  writeLine(
    `Import Lens budgets passed for ${files.length} changed ${files.length === 1 ? "file" : "files"}.`,
  );
  return 0;
};

const unmeasurableLine = ({ relative, transient, degraded, error }) => {
  const stages = [...new Set(transient.map((item) => item.stage))].sort().join(", ");
  const count = transient.length;

  // The aggregate failed OUTRIGHT: no bytes at all, for this one file. It used to throw out of
  // `analyzeFileWithDaemon` and abandon the whole run with exit 2 — one unmeasurable file taking
  // every other changed file's budget with it, and reporting a code that means "the CLI broke"
  // rather than the one FR-032a mandates for "could not measure". One bad file is one bad file.
  if (error) {
    return `${relative}: could not measure this file (${error}) - budget not evaluated`;
  }

  if (count > 0) {
    return `${relative}: could not measure ${count} ${count === 1 ? "import" : "imports"} (stage: ${stages}); file total is a floor - budget not evaluated`;
  }

  if (degraded) {
    // Not a floor: the opposite. The imports were measured; the file's own combined build was not,
    // so the number is a sum of per-import costs with shared modules counted once per import.
    return `${relative}: the file's combined build failed, so its total is an un-deduplicated sum of its imports and not the file's size - budget not evaluated`;
  }

  return `${relative}: an import that belongs in this file's total was not measured; file total is a floor - budget not evaluated`;
};

const main = async () => {
  const args = parseCliArgs(process.argv.slice(2));
  const cwd = process.cwd();
  const budgets = await loadBudgetConfig({ configPath: args.configPath });
  const { topLevel, files } = hasBudgets(budgets)
    ? await changedFiles(cwd)
    : { topLevel: cwd, files: [] };
  const supportedFiles = files.filter((filePath) =>
    supportedExtensions.has(path.extname(filePath)),
  );
  let daemon;

  try {
    if (!hasBudgets(budgets) || supportedFiles.length === 0) {
      process.exitCode = await runImportLensCheck({
        cwd,
        budgets,
        changedFiles: async () => supportedFiles,
        analyzeFile: async (filePath) => ({
          filePath,
          brotliBytes: 0,
          error: null,
          incomplete: false,
          degraded: false,
          diagnostics: [],
          unmeasured: [],
          imports: [],
        }),
      });
      return;
    }

    daemon = await startDaemon(topLevel);
    const exitCode = await runImportLensCheck({
      cwd,
      resolveRoot: topLevel,
      budgets,
      changedFiles: async () => supportedFiles,
      analyzeFile: (filePath) => analyzeFileWithDaemon(filePath, topLevel, daemon),
    });
    process.exitCode = exitCode;
  } finally {
    await daemon?.shutdown();
  }
};

const findDefaultBudgetConfig = async (readText) => {
  const rcPath = path.resolve(".importlensrc.json");
  if (existsSync(rcPath)) {
    return { path: rcPath, text: await readText(rcPath) };
  }

  const packageJsonPath = path.resolve("package.json");
  if (existsSync(packageJsonPath)) {
    return { path: packageJsonPath, text: await readText(packageJsonPath) };
  }

  return null;
};

const changedFiles = async (cwd) => {
  // `git diff --name-only` prints repository-root-relative paths regardless of
  // cwd, so file resolution must anchor at the git top level, not the
  // invocation directory (budget discovery stays cwd-scoped).
  const [{ stdout: diff }, { stdout: topLevel }] = await Promise.all([
    execFile("git", ["diff", "--name-only", "--diff-filter=ACMRTUXB", "HEAD", "--"], { cwd }),
    execFile("git", ["rev-parse", "--show-toplevel"], { cwd }),
  ]);

  return {
    topLevel: topLevel.trim(),
    files: diff.split(/\r?\n/u).filter(Boolean),
  };
};

/**
 * One file's raw wire response, flattened into the fields the gate reads. It issues no verdict and
 * throws for no file: an aggregate that **failed outright** (`error: Some` — nothing could be sized)
 * used to throw here, which `main` catches into exit **2**, abandoning every other changed file's
 * budget with it. One unmeasurable file is one unmeasurable file: it is reported as such, the run
 * continues, and the verdict is the exit 3 that FR-032a mandates for "could not measure".
 */
export const analyzeFileWithDaemon = async (filePath, workspaceRoot, daemon) => {
  const source = await readFile(filePath, "utf8");
  const response = await daemon.request({
    type: "file_size_document",
    version: protocolVersion,
    request_id: Date.now(),
    workspace_root: workspaceRoot,
    active_document_path: filePath,
    source,
    // CI budget checks must judge against the true current size — never a
    // stale-while-revalidate value — so force a synchronous fresh recompute.
    force_fresh: true,
  });

  // "Is there a size?", never "is there an error?". The old filter was `!item.error`, and a
  // transiently-degraded import carried `error: null` PLUS a fabricated size, so it sailed
  // through — measured against the wrong number, or dropped from the file total entirely.
  const imports = response.imports ?? [];
  const measured = imports.filter((item) => typeof item.brotli_bytes === "number");
  const unmeasured = imports
    .filter((item) => typeof item.brotli_bytes !== "number")
    .map((item) => {
      const stage = item.unmeasured_stage ?? "unknown";
      return { specifier: item.specifier, stage, transient: transientStages.has(stage) };
    });

  return {
    filePath,
    brotliBytes: response.brotli_bytes,
    // The three fields `isUsableFileSize` asks about, carried through verbatim rather than collapsed
    // into a verdict here. The verdict belongs to the gate, and the gate belongs to `runImportLensCheck`.
    error: response.error ?? null,
    // The daemon's own word for "an import that belongs in this total was never measured", which the
    // client cannot re-derive: a still-`loading` import leaves no failure of any stage behind.
    incomplete: response.incomplete === true,
    // And for "the file's own combined build failed", which `incomplete` cannot see at all.
    degraded: response.degraded === true,
    diagnostics: response.diagnostics ?? [],
    unmeasured,
    imports: measured.map((item) => ({
      specifier: item.specifier,
      brotliBytes: item.brotli_bytes,
    })),
  };
};

/**
 * Whether a file's totals are a measurement of THAT FILE, and so may be judged against a budget.
 *
 * ADR-0006 invariant 3 names "any pass/fail verdict" a durable store, so this is the same gate the
 * L1 aggregate cache applies (`FileSizeComputation::is_cacheable`, Rust) and the extension applies
 * before persisting a bundle-impact row (`isDurableFileSize`, TypeScript). It is stated a third time
 * here only because this CLI ships standalone and can import neither — the same forced duplication
 * as `transientStages` below, and held to the same standard: the three are kept in lockstep by a
 * drift check (`scripts/test/file-size-usability-coordination.test.mjs`), so a field added to one
 * and forgotten in the others fails the build rather than shipping a fourth instance of this defect.
 *
 * `degraded` is the one that was missing, and it is the one a CI gate is most likely to meet: a
 * file's combined build is the biggest build in the system, so it is the likeliest to hit the
 * daemon's build timeout — and when it does, every import can still be perfectly Measured, leaving
 * `incomplete: false`, `error: null`, and an un-deduplicated per-import sum in `brotli_bytes`.
 */
export const isUsableFileSize = (response) =>
  !response.error &&
  response.incomplete !== true &&
  response.degraded !== true &&
  !(response.diagnostics ?? []).some((item) => transientStages.has(item.stage));

// Mirrors `stage::is_transient` in daemon/src/engine/mod.rs (and `transientEngineStages` in
// extension/src/analysis/transience.ts). The three of them are kept in step by the drift check in
// scripts/test/engine-stage-coordination.test.mjs: a stage the daemon will not cache is a stage
// this gate must not judge from.
const transientStages = new Set(["timeout", "panic", "engine_gone"]);

const startDaemon = async (workspaceRoot) => {
  const target = platformTarget();
  const binary = daemonBinaryPath({ platformTarget: target });

  if (!existsSync(binary)) {
    throw new Error(`Import Lens daemon binary is unavailable at ${binary}`);
  }

  const { cachePath, lifecyclePath } = resolveCliStoragePaths();
  await mkdir(cachePath, { recursive: true });
  await mkdir(lifecyclePath, { recursive: true });
  const pipeName =
    process.platform === "win32"
      ? `\\\\.\\pipe\\import-lens-cli-${process.pid}-${randomUUID()}`
      : path.join(lifecyclePath, `import-lens-cli-${process.pid}-${randomUUID()}.sock`);
  const child = spawn(
    binary,
    ["--pipe", pipeName, "--workspace", workspaceRoot, "--storage", lifecyclePath],
    { stdio: ["ignore", "ignore", "inherit"] },
  );
  let socket;
  let client;

  try {
    socket = await connectWithRetry(pipeName, 5000);
    client = createDaemonClient(socket);
    client.send({
      type: "hello",
      version: protocolVersion,
      workspace_root: workspaceRoot,
      storage_path: cachePath,
      enable_disk_cache: true,
      cache_max_size_mb: 512,
      // Deprecated and ignored by the daemon (the byte budget governs capacity);
      // kept on the wire for Hello frame compatibility.
      cache_max_age_days: 30,
      log_level: "warn",
    });
  } catch (error) {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill();
    }
    socket?.destroy();
    throw error;
  }

  return {
    request: client.request,
    shutdown: async () => {
      try {
        client.send({ type: "shutdown" });
      } catch {
        // best effort shutdown
      }
      socket.destroy();
      if (child.exitCode === null && child.signalCode === null) {
        child.kill();
      }
    },
  };
};

const cliPackageRoot = () => path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

export const resolveCliStoragePaths = ({
  env = process.env,
  platform = process.platform,
  homeDir = os.homedir(),
} = {}) => {
  const basePath =
    platform === "win32"
      ? path.join(env.LOCALAPPDATA ?? path.join(homeDir, "AppData", "Local"), "ImportLens")
      : platform === "darwin"
        ? path.join(homeDir, "Library", "Caches", "ImportLens")
        : path.join(env.XDG_CACHE_HOME ?? path.join(homeDir, ".cache"), "import-lens");

  return {
    cachePath: path.join(basePath, "daemon-cache"),
    lifecyclePath: path.join(basePath, "daemon-lifecycle"),
  };
};

export const daemonBinaryPath = ({
  packageRoot = cliPackageRoot(),
  platformTarget: requestedTarget = platformTarget(),
} = {}) => {
  if (!requestedTarget) {
    throw new Error(
      `Unsupported platform for Import Lens daemon: ${process.platform}-${os.arch()}`,
    );
  }

  return path.join(packageRoot, DAEMON_ROOT, requestedTarget, daemonBinaryName(requestedTarget));
};

// Where the daemon binaries live relative to the package root. Mirrors
// daemonRoot in scripts/targets.mjs (this CLI ships standalone and cannot
// import build scripts); the daemon-path-contract test keeps them in lockstep.
const DAEMON_ROOT = "dist/bin";

const daemonBinaryName = (target) =>
  target.startsWith("win32-") ? "import-lens-daemon.exe" : "import-lens-daemon";

const connectWithRetry = async (pipeName, timeoutMs) => {
  const started = Date.now();
  let lastError;

  while (Date.now() - started < timeoutMs) {
    try {
      return await new Promise((resolve, reject) => {
        const socket = net.createConnection(pipeName);
        socket.once("connect", () => resolve(socket));
        socket.once("error", (error) => {
          socket.destroy();
          reject(error);
        });
      });
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }

  throw lastError ?? new Error("failed to connect to Import Lens daemon");
};

export const createDaemonClient = (socket) => {
  const pending = new Map();
  let buffer = Buffer.alloc(0);

  socket.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);

    while (buffer.length >= 4) {
      const length = buffer.readUInt32BE(0);
      if (buffer.length < length + 4) {
        return;
      }

      const payload = buffer.subarray(4, 4 + length);
      buffer = buffer.subarray(4 + length);
      let message;

      try {
        message = decode(payload);
      } catch (error) {
        rejectPending(error instanceof Error ? error : new Error(String(error)));
        socket.destroy?.();
        return;
      }

      const requestId = requestIdForMessage(message);

      if (requestId === null) {
        continue;
      }

      const item = pending.get(requestId);

      if (!item) {
        continue;
      }

      pending.delete(requestId);
      clearTimeout(item.timer);
      item.resolve(message);
    }
  });
  socket.on("error", (error) => rejectPending(error));
  socket.on("close", () => rejectPending(new Error("IPC socket closed")));

  const send = (message) => {
    const payload = Buffer.from(encode(message));
    const header = Buffer.allocUnsafe(4);
    header.writeUInt32BE(payload.length, 0);
    socket.write(Buffer.concat([header, payload]));
  };

  return {
    send,
    request: (message, timeoutMs = defaultIpcTimeoutMs) => {
      const requestId = requestIdForMessage(message);

      if (requestId === null) {
        return Promise.reject(new Error("IPC request requires a numeric request_id"));
      }

      if (pending.has(requestId)) {
        return Promise.reject(new Error(`duplicate IPC request id ${requestId}`));
      }

      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          if (pending.delete(requestId)) {
            reject(new Error(`IPC request ${requestId} timed out after ${timeoutMs}ms`));
          }
        }, timeoutMs);

        pending.set(requestId, { resolve, reject, timer });

        try {
          send(message);
        } catch (error) {
          clearTimeout(timer);
          pending.delete(requestId);
          reject(error);
        }
      });
    },
  };

  function rejectPending(error) {
    for (const item of pending.values()) {
      clearTimeout(item.timer);
      item.reject(error);
    }

    pending.clear();
  }
};

const requestIdForMessage = (message) => {
  if (!message || typeof message !== "object" || !Number.isSafeInteger(message.request_id)) {
    return null;
  }

  return message.request_id;
};

const sanitizeBudgets = (value) => {
  if (!value || typeof value !== "object") {
    return {};
  }

  const budgets = {};
  if (
    typeof value.perImportBrotliBytes === "number" &&
    Number.isFinite(value.perImportBrotliBytes) &&
    value.perImportBrotliBytes > 0
  ) {
    budgets.perImportBrotliBytes = Math.floor(value.perImportBrotliBytes);
  }
  if (
    typeof value.perFileBrotliBytes === "number" &&
    Number.isFinite(value.perFileBrotliBytes) &&
    value.perFileBrotliBytes > 0
  ) {
    budgets.perFileBrotliBytes = Math.floor(value.perFileBrotliBytes);
  }
  return budgets;
};

const hasBudgets = (budgets) =>
  budgets.perImportBrotliBytes !== undefined || budgets.perFileBrotliBytes !== undefined;

const platformTarget = () => {
  const key = `${process.platform}-${os.arch()}`;
  const targets = {
    "win32-x64": "win32-x64",
    "win32-arm64": "win32-arm64",
    "linux-x64": "linux-x64",
    "linux-arm64": "linux-arm64",
    "darwin-x64": "darwin-x64",
    "darwin-arm64": "darwin-arm64",
  };
  const target = targets[key];

  if (!target) {
    throw new Error(`Unsupported platform for Import Lens daemon: ${key}`);
  }

  return target;
};

const formatBytes = (bytes) => {
  if (bytes < 1000) {
    return `${bytes} B`;
  }

  return `${(bytes / 1000).toFixed(1)} kB`;
};

const usage = () => "Usage: importlens check [--config <path>]";

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 2;
  });
}
