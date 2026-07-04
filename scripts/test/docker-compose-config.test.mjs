import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

test("docker build script delegates to the Compose builder service", () => {
  const manifest = JSON.parse(readFileSync(new URL("../../package.json", import.meta.url), "utf8"));

  assert.equal(manifest.scripts["docker:build"], "docker compose run --rm --build builder");
});

test("Compose builder keeps pnpm noninteractive and isolated from host installs", () => {
  const compose = readFileSync(new URL("../../compose.yaml", import.meta.url), "utf8");

  assert.match(compose, /dockerfile:\s*Dockerfile\.build/u);
  assert.match(compose, /CI:\s*"true"/u);
  assert.match(compose, /PNPM_CONFIG_CONFIRM_MODULES_PURGE:\s*"false"/u);
  assert.match(compose, /IMPORT_LENS_PERF_MULTIPLIER:\s*\$\{IMPORT_LENS_PERF_MULTIPLIER:-20\}/u);
  assert.match(compose, /-\s+\/workspace\/node_modules/u);
  assert.match(compose, /-\s+\/workspace\/\.pnpm-store/u);
});

test("Docker build cross-compiles the four unix targets with zig", () => {
  const entrypoint = readFileSync(
    new URL("../../scripts/docker-build-entrypoint.sh", import.meta.url),
    "utf8",
  );

  assert.match(
    entrypoint,
    /for target in linux-x64 linux-arm64 darwin-x64 darwin-arm64; do\n {2}node scripts\/package-target\.mjs "\$target" --zigbuild/u,
  );
});

test("Docker build cross-compiles both Windows MSVC targets with cargo-xwin", () => {
  const entrypoint = readFileSync(
    new URL("../../scripts/docker-build-entrypoint.sh", import.meta.url),
    "utf8",
  );

  // zig cannot emit the MSVC ABI, so Windows takes the --xwin path instead.
  assert.match(
    entrypoint,
    /for target in win32-x64 win32-arm64; do\n {2}node scripts\/package-target\.mjs "\$target" --xwin/u,
  );
  assert.match(entrypoint, /import-lens-win32-x64-\$\{version\}\.vsix/u);
  assert.match(entrypoint, /import-lens-win32-arm64-\$\{version\}\.vsix/u);
});

test("Dockerfile installs the cargo-xwin Windows cross toolchain", () => {
  const dockerfile = readFileSync(new URL("../../Dockerfile.build", import.meta.url), "utf8");

  assert.match(dockerfile, /^ENV XWIN_ACCEPT_LICENSE=1$/mu);
  assert.match(dockerfile, /rustup target add[\s\S]*x86_64-pc-windows-msvc/u);
  assert.match(dockerfile, /rustup target add[\s\S]*aarch64-pc-windows-msvc/u);
  assert.match(dockerfile, /apt-get install[^\n]*\bclang\b/u);
  assert.match(dockerfile, /apt-get install[^\n]*\blld\b/u);
  // llvm supplies llvm-lib, without which cc-rs cannot archive the C deps.
  assert.match(dockerfile, /apt-get install[^\n]*\bllvm\b/u);
  assert.match(dockerfile, /cargo install cargo-xwin --locked/u);
});
