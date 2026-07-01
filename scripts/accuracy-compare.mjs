#!/usr/bin/env node

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { brotliCompressSync, constants as zlibConstants } from "node:zlib";
import { decode, encode } from "@msgpack/msgpack";
import * as esbuild from "esbuild";

const protocolVersion = 6;
const packageName = "importlens-accuracy-fixture";
const tolerance = Number(process.env.IMPORT_LENS_ACCURACY_TOLERANCE ?? "0.75");

const main = async () => {
  const workspace = await mkdtemp(path.join(os.tmpdir(), "importlens-accuracy-"));
  let daemon;

  try {
    const fixture = await writeFixture(workspace);
    daemon = await startDaemon(workspace);
    const benchmarks = [
      {
        label: "same-module unused export",
        activeDocumentPath: fixture.flatActiveDocumentPath,
        named: "light",
      },
      {
        label: "branchy unused export dependency",
        activeDocumentPath: fixture.branchyActiveDocumentPath,
        named: "used",
        excludedModule: "/huge.js",
      },
    ];

    for (const [index, benchmark] of benchmarks.entries()) {
      const importLens = await importLensNamedSize(
        daemon,
        workspace,
        benchmark.activeDocumentPath,
        benchmark.named,
        index + 1,
      );
      const esbuildSize = await esbuildNamedSize(workspace, benchmark.activeDocumentPath);
      const delta = Math.abs(importLens.brotliBytes - esbuildSize.brotliBytes);
      const relativeDelta = delta / Math.max(esbuildSize.brotliBytes, 1);

      process.stdout.write([
        `${benchmark.label}:`,
        `  ImportLens named import: ${importLens.brotliBytes} B br (${importLens.minifiedBytes} B minified)`,
        `  esbuild named import: ${esbuildSize.brotliBytes} B br (${esbuildSize.minifiedBytes} B minified)`,
        `  relative delta: ${(relativeDelta * 100).toFixed(1)}%`,
      ].join("\n"));
      process.stdout.write("\n");

      if (relativeDelta > tolerance) {
        throw new Error(`${benchmark.label} accuracy delta ${(relativeDelta * 100).toFixed(1)}% exceeds ${(tolerance * 100).toFixed(1)}% tolerance`);
      }

      if (benchmark.excludedModule && importLens.moduleBreakdown.some((module) =>
        module.path.replaceAll("\\", "/").endsWith(benchmark.excludedModule)
      )) {
        throw new Error(`${benchmark.label} unexpectedly included ${benchmark.excludedModule}`);
      }
    }
  } finally {
    await daemon?.shutdown();
    await rm(workspace, { recursive: true, force: true });
  }
};

const writeFixture = async (workspace) => {
  const packageRoot = path.join(workspace, "node_modules", packageName);
  const sourceRoot = path.join(workspace, "src");
  await mkdir(packageRoot, { recursive: true });
  await mkdir(sourceRoot, { recursive: true });

  await writeFile(
    path.join(packageRoot, "package.json"),
    JSON.stringify({
      name: packageName,
      version: "1.0.0",
      type: "module",
      module: "index.js",
      sideEffects: false,
    }, null, 2),
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "index.js"),
    [
      `import { small } from "./small.js";`,
      `import { huge } from "./huge.js";`,
      `export const light = ${JSON.stringify(deterministicPayload(12_000))};`,
      `export const unusedFlat = ${JSON.stringify(deterministicPayload(120_000))};`,
      `export const used = small;`,
      `export const unusedBranch = huge;`,
    ].join("\n"),
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "small.js"),
    `export const small = ${JSON.stringify(deterministicPayload(12_000))};\n`,
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "huge.js"),
    `export const huge = ${JSON.stringify(deterministicPayload(180_000))};\n`,
    "utf8",
  );

  const flatActiveDocumentPath = path.join(sourceRoot, "flat-entry.js");
  const branchyActiveDocumentPath = path.join(sourceRoot, "branchy-entry.js");
  await writeFile(flatActiveDocumentPath, `export { light } from "${packageName}";\n`, "utf8");
  await writeFile(branchyActiveDocumentPath, `export { used } from "${packageName}";\n`, "utf8");
  return { flatActiveDocumentPath, branchyActiveDocumentPath };
};

const importLensNamedSize = async (daemon, workspace, activeDocumentPath, named, requestId) => {
  const response = await daemon.request({
    version: protocolVersion,
    request_id: requestId,
    workspace_root: workspace,
    active_document_path: activeDocumentPath,
    imports: [{
      specifier: packageName,
      package: packageName,
      version: "1.0.0",
      named: [named],
      import_kind: "named",
      runtime: "component",
    }],
  });
  const result = response.imports?.[0];

  if (!result || result.error) {
    throw new Error(`ImportLens accuracy request failed: ${result?.error ?? "missing result"}`);
  }

  return {
    brotliBytes: result.brotli_bytes,
    minifiedBytes: result.minified_bytes,
    moduleBreakdown: result.module_breakdown ?? [],
  };
};

const esbuildNamedSize = async (workspace, activeDocumentPath) => {
  const result = await esbuild.build({
    absWorkingDir: workspace,
    entryPoints: [activeDocumentPath],
    bundle: true,
    minify: true,
    write: false,
    format: "esm",
    platform: "browser",
    treeShaking: true,
    logLevel: "silent",
  });
  const output = result.outputFiles[0]?.contents;

  if (!output) {
    throw new Error("esbuild did not produce an output file");
  }

  return {
    brotliBytes: brotliSize(output),
    minifiedBytes: output.length,
  };
};

const startDaemon = async (workspace) => {
  const storagePath = path.join(workspace, ".importlens-cache");
  await mkdir(storagePath, { recursive: true });
  const pipeName = process.platform === "win32"
    ? `\\\\.\\pipe\\import-lens-accuracy-${process.pid}-${randomUUID()}`
    : path.join(os.tmpdir(), `import-lens-accuracy-${process.pid}-${randomUUID()}.sock`);
  const child = spawn("cargo", [
    "run",
    "--quiet",
    "--bin",
    "import-lens-daemon",
    "--",
    "--pipe",
    pipeName,
    "--workspace",
    workspace,
    "--storage",
    storagePath,
  ], {
    cwd: fileURLToPath(new URL("..", import.meta.url)),
    stdio: ["ignore", "ignore", "pipe"],
  });
  const stderr = [];
  child.stderr.on("data", (chunk) => stderr.push(chunk.toString()));

  try {
    const socket = await connectWithRetry(pipeName, 60_000, child, stderr);
    const client = daemonClient(socket);
    client.send({
      type: "hello",
      version: protocolVersion,
      workspace_root: workspace,
      storage_path: storagePath,
      enable_disk_cache: false,
      cache_max_size_mb: 512,
      cache_max_age_days: 30,
      log_level: "warn",
    });

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
  } catch (error) {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill();
    }
    throw error;
  }
};

const connectWithRetry = async (pipeName, timeoutMs, child, stderr) => {
  const started = Date.now();
  let lastError;

  while (Date.now() - started < timeoutMs) {
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`daemon exited before IPC connection: ${stderr.join("").trim()}`);
    }

    try {
      return await new Promise((resolve, reject) => {
        const socket = net.createConnection(pipeName);
        socket.once("connect", () => resolve(socket));
        socket.once("error", reject);
      });
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }

  const stderrText = stderr.join("").trim();
  const suffix = stderrText ? `; daemon stderr: ${stderrText}` : "";
  throw new Error(`failed to connect to ImportLens daemon: ${lastError?.message ?? "timeout"}${suffix}`);
};

const daemonClient = (socket) => {
  const pending = [];
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
      pending.shift()?.resolve(decode(payload));
    }
  });
  socket.on("error", (error) => {
    for (const item of pending) {
      item.reject(error);
    }
    pending.length = 0;
  });

  const send = (message) => {
    const payload = Buffer.from(encode(message));
    const header = Buffer.allocUnsafe(4);
    header.writeUInt32BE(payload.length, 0);
    socket.write(Buffer.concat([header, payload]));
  };

  return {
    send,
    request: (message) => new Promise((resolve, reject) => {
      pending.push({ resolve, reject });
      send(message);
    }),
  };
};

const deterministicPayload = (length) => {
  const alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
  let value = "";
  let state = 0x12345678;

  for (let index = 0; index < length; index += 1) {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    value += alphabet[state % alphabet.length];
  }

  return value;
};

const brotliSize = (source) =>
  brotliCompressSync(source, {
    params: {
      [zlibConstants.BROTLI_PARAM_QUALITY]: 11,
    },
  }).length;

main().catch((error) => {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
});
