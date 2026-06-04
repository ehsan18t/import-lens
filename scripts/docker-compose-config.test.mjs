import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

test("docker build script delegates to the Compose builder service", () => {
  const manifest = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));

  assert.equal(manifest.scripts["docker:build"], "docker compose run --rm --build builder");
});

test("Compose builder keeps pnpm noninteractive and isolated from host installs", () => {
  const compose = readFileSync(new URL("../compose.yaml", import.meta.url), "utf8");

  assert.match(compose, /dockerfile:\s*Dockerfile\.build/u);
  assert.match(compose, /CI:\s*"true"/u);
  assert.match(compose, /PNPM_CONFIG_CONFIRM_MODULES_PURGE:\s*"false"/u);
  assert.match(compose, /IMPORT_LENS_PERF_MULTIPLIER:\s*\$\{IMPORT_LENS_PERF_MULTIPLIER:-20\}/u);
  assert.match(compose, /-\s+\/workspace\/node_modules/u);
  assert.match(compose, /-\s+\/workspace\/\.pnpm-store/u);
});
