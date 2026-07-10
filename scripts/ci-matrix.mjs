#!/usr/bin/env node

import { platformTargets, targetInfo } from "./targets.mjs";

// Which runner image builds which target is CI-only knowledge, so it lives here
// rather than in targets.mjs. Everything else in the matrix is derived, and an
// unmapped target fails the job instead of silently running on `undefined`.
const runners = {
  "win32-x64": "windows-latest",
  "win32-arm64": "windows-latest",
  "linux-x64": "ubuntu-24.04",
  "linux-arm64": "ubuntu-24.04-arm",
  "darwin-x64": "macos-latest",
  "darwin-arm64": "macos-latest",
};

const runnerFor = (target) => {
  const runner = runners[target];

  if (!runner) {
    throw new Error(`No CI runner is mapped for target: ${target}`);
  }

  return runner;
};

const include = platformTargets.map((target) => ({
  target,
  runner: runnerFor(target),
  "rust-target": targetInfo(target).rustTarget,
}));

console.log(JSON.stringify({ include }));
