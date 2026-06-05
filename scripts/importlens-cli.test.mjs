import assert from "node:assert/strict";
import test from "node:test";
import {
  loadBudgetConfig,
  parseCliArgs,
  runImportLensCheck,
} from "../cli/importlens.mjs";

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
