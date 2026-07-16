#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

const fail = (message) => {
  console.error(message);
  process.exit(1);
};

const run = (command, args) => {
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    stdio: "inherit",
  });

  if (result.error) {
    fail(result.error.message);
  }

  if (result.status !== 0) {
    fail(`${command} ${args.join(" ")} failed with exit code ${result.status ?? "unknown"}`);
  }
};

const probe = spawnSync("cargo", ["llvm-cov", "--version"], {
  cwd: repoRoot,
  stdio: "pipe",
});

if (probe.status !== 0) {
  fail(
    "cargo-llvm-cov is required for the Rust coverage gate. Install it with: cargo install cargo-llvm-cov --version 0.8.7 --locked",
  );
}

run("cargo", ["llvm-cov", "--workspace", "--locked", "--fail-under-lines", "70"]);
