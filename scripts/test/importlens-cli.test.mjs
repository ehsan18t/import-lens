import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import path from "node:path";
import { encode } from "@msgpack/msgpack";
import test from "node:test";
import {
  createDaemonClient,
  daemonBinaryPath,
  loadBudgetConfig,
  parseCliArgs,
  resolveCliStoragePaths,
  runImportLensCheck,
} from "../../cli/importlens.mjs";

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
    () => loadBudgetConfig({
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
    analyzeFile: async () => ({
      filePath: "/workspace/src/app.ts",
      brotliBytes: 2500,
      imports: [
        { specifier: "large-lib", brotliBytes: 1500 },
        { specifier: "small-lib", brotliBytes: 500 },
      ],
    }),
    writeLine: (line) => output.push(line),
  });

  assert.equal(exitCode, 1);
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
    analyzeFile: async () => ({
      filePath: "/workspace/src/app.ts",
      brotliBytes: 1200,
      imports: [{ specifier: "small-lib", brotliBytes: 900 }],
    }),
    writeLine: (line) => output.push(line),
  });

  assert.equal(exitCode, 0);
  assert.deepEqual(output, ["ImportLens budgets passed for 1 changed file."]);
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
      return { filePath, brotliBytes: 2500, imports: [] };
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
    path.join("C:", "ImportLens", "bin", "win32-x64", "import-lens-daemon.exe"),
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
