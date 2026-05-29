#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const docker = process.platform === "win32" ? "docker.exe" : "docker";
const manifest = JSON.parse(readFileSync(path.join(repoRoot, "package.json"), "utf8"));
const imageTag = `import-lens-builder:${manifest.version}`;
const forwardedEnv = ["IMPORT_LENS_PERF_MULTIPLIER"]
  .filter((name) => process.env[name])
  .flatMap((name) => ["--env", `${name}=${process.env[name]}`]);

const run = (args) => {
  const result = spawnSync(docker, args, {
    cwd: repoRoot,
    stdio: "inherit",
  });

  if (result.error) {
    console.error(result.error.message);
    process.exit(1);
  }

  if (result.status !== 0) {
    console.error(`docker ${args.join(" ")} failed with exit code ${result.status ?? "unknown"}`);
    process.exit(result.status ?? 1);
  }
};

if (manifest.icon && !existsSync(path.join(repoRoot, manifest.icon))) {
  console.error(`Extension icon is declared at ${manifest.icon}, but the file does not exist.`);
  process.exit(1);
}

run(["build", "--file", "Dockerfile.build", "--tag", imageTag, "."]);
run([
  "run",
  "--rm",
  ...forwardedEnv,
  "--volume",
  `${repoRoot}:/workspace`,
  "--workdir",
  "/workspace",
  imageTag,
]);
