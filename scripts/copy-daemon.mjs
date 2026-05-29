#!/usr/bin/env node

import { cpSync, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const target = process.argv[2];

if (!target) {
  console.error("Usage: node scripts/copy-daemon.mjs <target>");
  process.exit(1);
}

const binaryName = target.startsWith("win32") ? "import-lens-daemon.exe" : "import-lens-daemon";

// When cross-compiling, Cargo puts the binary in target/<cross-target>/release/
// But wait! If we cross-compile with cargo zigbuild, we specify the target triple.
// Our pnpm script just runs `cargo build --release`, which puts it in `target/release/`.
// To support both, we should check `target/release` and `target/<triple>/release`.

const rustTriple = (() => {
  switch (target) {
    case "win32-x64": return "x86_64-pc-windows-msvc";
    case "win32-arm64": return "aarch64-pc-windows-msvc";
    case "linux-x64": return "x86_64-unknown-linux-gnu";
    case "linux-arm64": return "aarch64-unknown-linux-gnu";
    case "darwin-x64": return "x86_64-apple-darwin";
    case "darwin-arm64": return "aarch64-apple-darwin";
    default: return null;
  }
})();

const possibleSources = [
  path.join(repoRoot, "target", rustTriple ?? "", "release", binaryName),
  path.join(repoRoot, "target", "release", binaryName),
];

const source = possibleSources.find((p) => existsSync(p));

if (!source) {
  console.error(`Daemon binary not found. Expected it at one of:\n${possibleSources.join("\n")}`);
  console.error("Run cargo build first.");
  process.exit(1);
}

const targetDir = path.join(repoRoot, "bin", target);
const destination = path.join(targetDir, binaryName);

mkdirSync(targetDir, { recursive: true });
cpSync(source, destination, { force: true });
console.log(`Copied ${source} to ${destination}`);
