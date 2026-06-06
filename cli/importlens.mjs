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
import {
  ExportImportNameKind,
  ImportNameKind,
  parseSync,
} from "oxc-parser";

const execFile = promisify(execFileCallback);
const protocolVersion = 4;
const supportedExtensions = new Set([".js", ".jsx", ".ts", ".tsx", ".mjs", ".mts", ".cjs", ".cts"]);
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
    const result = await analyzeFile(path.resolve(cwd, filePath));
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
  const files = hasBudgets(budgets) ? await changedFiles(cwd) : [];
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

    daemon = await startDaemon(cwd);
    const exitCode = await runImportLensCheck({
      cwd,
      budgets,
      changedFiles: async () => supportedFiles,
      analyzeFile: (filePath) => analyzeFileWithDaemon(filePath, cwd, daemon),
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
  const { stdout } = await execFile("git", ["diff", "--name-only", "--diff-filter=ACMRTUXB", "HEAD", "--"], { cwd });
  return stdout.split(/\r?\n/u).filter(Boolean);
};

const analyzeFileWithDaemon = async (filePath, workspaceRoot, daemon) => {
  const source = await readFile(filePath, "utf8");
  const imports = [];

  for (const detected of extractRuntimeImports(filePath, source)) {
    const version = await resolveInstalledVersion(detected.packageName, filePath);
    if (version) {
      imports.push({
        specifier: detected.specifier,
        package: detected.packageName,
        version,
        named: detected.named,
        import_kind: detected.importKind,
        runtime: "component",
      });
    }
  }

  if (imports.length === 0) {
    return { filePath, brotliBytes: 0, imports: [] };
  }

  const response = await daemon.request({
    type: "file_size",
    version: protocolVersion,
    request_id: Date.now(),
    workspace_root: workspaceRoot,
    active_document_path: filePath,
    imports,
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

  const storagePath = path.resolve(".importlens", "cache");
  await mkdir(storagePath, { recursive: true });
  const pipeName = process.platform === "win32"
    ? `\\\\.\\pipe\\import-lens-cli-${process.pid}-${randomUUID()}`
    : path.join(storagePath, `import-lens-cli-${process.pid}-${randomUUID()}.sock`);
  const child = spawn(binary, [
    "--pipe",
    pipeName,
    "--workspace",
    workspaceRoot,
    "--storage",
    storagePath,
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
      storage_path: storagePath,
      enable_disk_cache: true,
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

const extractRuntimeImports = (filePath, source) => {
  const parsed = parseSync(filePath, source, {
    sourceType: "module",
    astType: "ts",
    lang: filePath.endsWith("x") ? "tsx" : "ts",
  });
  const imports = [];

  for (const item of parsed.module.staticImports) {
    const specifier = item.moduleRequest.value;
    if (!isRuntimePackageSpecifier(specifier)) {
      continue;
    }

    const entries = item.entries.filter((entry) => !entry.isType);
    const named = entries
      .filter((entry) => entry.importName.kind === ImportNameKind.Name && entry.importName.name)
      .map((entry) => entry.importName.name)
      .sort();

    if (entries.length === 0 && item.entries.length === 0) {
      imports.push(detected(specifier, "namespace"));
    }
    if (entries.some((entry) => entry.importName.kind === ImportNameKind.Default)) {
      imports.push(detected(specifier, "default"));
    }
    if (entries.some((entry) => entry.importName.kind === ImportNameKind.NamespaceObject)) {
      imports.push(detected(specifier, "namespace"));
    }
    if (named.length > 0) {
      imports.push(detected(specifier, "named", named));
    }
  }

  for (const item of parsed.module.staticExports) {
    const moduleRequest = item.entries.find((entry) => entry.moduleRequest)?.moduleRequest;
    if (!moduleRequest || !isRuntimePackageSpecifier(moduleRequest.value)) {
      continue;
    }

    const named = item.entries
      .filter((entry) => entry.importName.kind === ExportImportNameKind.Name && entry.importName.name)
      .map((entry) => entry.importName.name)
      .sort();

    if (item.entries.some((entry) => entry.importName.kind === ExportImportNameKind.All || entry.importName.kind === ExportImportNameKind.AllButDefault)) {
      imports.push(detected(moduleRequest.value, "namespace"));
    }
    if (named.length > 0) {
      imports.push(detected(moduleRequest.value, "named", named));
    }
  }

  for (const item of parsed.module.dynamicImports) {
    const specifier = literalDynamicImportSpecifier(source.slice(item.moduleRequest.start, item.moduleRequest.end));
    if (specifier && isRuntimePackageSpecifier(specifier)) {
      imports.push(detected(specifier, "dynamic"));
    }
  }

  return imports;
};

const detected = (specifier, importKind, named = []) => ({
  specifier,
  packageName: packageNameForSpecifier(specifier),
  importKind,
  named,
});

const literalDynamicImportSpecifier = (value) => {
  const first = value.at(0);
  const last = value.at(-1);
  if ((first === "'" || first === '"') && first === last) {
    return value.slice(1, -1);
  }
  if (first === "`" && last === "`" && !value.includes("${")) {
    return value.slice(1, -1);
  }
  return null;
};

const resolveInstalledVersion = async (packageName, activeFilePath) => {
  const packageJson = resolvePackageJson(packageName, path.dirname(activeFilePath));
  if (!packageJson) {
    return null;
  }

  try {
    const manifest = JSON.parse(await readFile(packageJson, "utf8"));
    return typeof manifest.version === "string" ? manifest.version : "unknown";
  } catch {
    return "unknown";
  }
};

const resolvePackageJson = (packageName, fromDirectory) => {
  let current = fromDirectory;

  while (true) {
    const candidate = path.join(current, "node_modules", packageName, "package.json");
    if (existsSync(candidate)) {
      return candidate;
    }

    const parent = path.dirname(current);
    if (parent === current) {
      return null;
    }
    current = parent;
  }
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

const packageNameForSpecifier = (specifier) => {
  if (specifier.startsWith("@")) {
    return specifier.split("/").slice(0, 2).join("/");
  }

  return specifier.split("/")[0];
};

const isRuntimePackageSpecifier = (specifier) =>
  Boolean(specifier)
  && !specifier.startsWith(".")
  && !specifier.startsWith("/")
  && !specifier.startsWith("node:")
  && !specifier.includes("://");

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
