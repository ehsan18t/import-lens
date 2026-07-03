#!/usr/bin/env node
import { spawn, execFile as execFileCallback } from "node:child_process";
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
const supportedExtensions = new Set([".js", ".jsx", ".ts", ".tsx", ".mjs", ".mts", ".cjs", ".cts", ".svelte", ".vue", ".astro"]);
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
    throw new Error(`failed to parse ${source.path}: ${error instanceof Error ? error.message : String(error)}`);
  }

  return sanitizeBudgets(parsed.budgets ?? parsed.importLens?.budgets ?? {});
};

export const runImportLensCheck = async ({
  cwd = process.cwd(),
  resolveRoot = cwd,
  budgets,
  changedFiles,
  analyzeFile,
  writeLine = (line) => process.stdout.write(`${line}\n`),
}) => {
  if (!hasBudgets(budgets)) {
    writeLine("No ImportLens budgets configured.");
    return 0;
  }

  const files = (await changedFiles())
    .filter((filePath) => supportedExtensions.has(path.extname(filePath)))
    .sort();
  const violations = [];

  for (const filePath of files) {
    const result = await analyzeFile(path.resolve(resolveRoot, filePath));
    const relative = path.relative(cwd, result.filePath).split(path.sep).join("/");

    if (budgets.perFileBrotliBytes !== undefined && result.brotliBytes > budgets.perFileBrotliBytes) {
      violations.push(`${relative}: file Brotli budget exceeded: ${formatBytes(result.brotliBytes)} > ${formatBytes(budgets.perFileBrotliBytes)}`);
    }

    for (const item of result.imports) {
      if (budgets.perImportBrotliBytes !== undefined && item.brotliBytes > budgets.perImportBrotliBytes) {
        violations.push(`${relative}: ${item.specifier} Brotli budget exceeded: ${formatBytes(item.brotliBytes)} > ${formatBytes(budgets.perImportBrotliBytes)}`);
      }
    }
  }

  if (violations.length > 0) {
    for (const violation of violations) {
      writeLine(violation);
    }
    return 1;
  }

  writeLine(`ImportLens budgets passed for ${files.length} changed ${files.length === 1 ? "file" : "files"}.`);
  return 0;
};

const main = async () => {
  const args = parseCliArgs(process.argv.slice(2));
  const cwd = process.cwd();
  const budgets = await loadBudgetConfig({ configPath: args.configPath });
  const { topLevel, files } = hasBudgets(budgets)
    ? await changedFiles(cwd)
    : { topLevel: cwd, files: [] };
  const supportedFiles = files.filter((filePath) => supportedExtensions.has(path.extname(filePath)));
  let daemon;

  try {
    if (!hasBudgets(budgets) || supportedFiles.length === 0) {
      process.exitCode = await runImportLensCheck({
        cwd,
        budgets,
        changedFiles: async () => supportedFiles,
        analyzeFile: async (filePath) => ({ filePath, brotliBytes: 0, imports: [] }),
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

const analyzeFileWithDaemon = async (filePath, workspaceRoot, daemon) => {
  const source = await readFile(filePath, "utf8");
  const response = await daemon.request({
    type: "file_size_document",
    version: protocolVersion,
    request_id: Date.now(),
    workspace_root: workspaceRoot,
    active_document_path: filePath,
    source,
  });

  if (response.error) {
    throw new Error(`ImportLens file-size request failed for ${filePath}: ${response.error}`);
  }

  return {
    filePath,
    brotliBytes: response.brotli_bytes,
    imports: response.imports
      .filter((item) => !item.error)
      .map((item) => ({
        specifier: item.specifier,
        brotliBytes: item.brotli_bytes,
      })),
  };
};

const startDaemon = async (workspaceRoot) => {
  const target = platformTarget();
  const binary = daemonBinaryPath({ platformTarget: target });

  if (!existsSync(binary)) {
    throw new Error(`ImportLens daemon binary is unavailable at ${binary}`);
  }

  const { cachePath, lifecyclePath } = resolveCliStoragePaths();
  await mkdir(cachePath, { recursive: true });
  await mkdir(lifecyclePath, { recursive: true });
  const pipeName = process.platform === "win32"
    ? `\\\\.\\pipe\\import-lens-cli-${process.pid}-${randomUUID()}`
    : path.join(lifecyclePath, `import-lens-cli-${process.pid}-${randomUUID()}.sock`);
  const child = spawn(binary, [
    "--pipe",
    pipeName,
    "--workspace",
    workspaceRoot,
    "--storage",
    lifecyclePath,
  ], { stdio: ["ignore", "ignore", "inherit"] });
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
  const basePath = platform === "win32"
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
    throw new Error(`Unsupported platform for ImportLens daemon: ${process.platform}-${os.arch()}`);
  }

  return path.join(packageRoot, "bin", requestedTarget, daemonBinaryName(requestedTarget));
};

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

  throw lastError ?? new Error("failed to connect to ImportLens daemon");
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
  if (typeof value.perImportBrotliBytes === "number" && Number.isFinite(value.perImportBrotliBytes) && value.perImportBrotliBytes > 0) {
    budgets.perImportBrotliBytes = Math.floor(value.perImportBrotliBytes);
  }
  if (typeof value.perFileBrotliBytes === "number" && Number.isFinite(value.perFileBrotliBytes) && value.perFileBrotliBytes > 0) {
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
    throw new Error(`Unsupported platform for ImportLens daemon: ${key}`);
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
