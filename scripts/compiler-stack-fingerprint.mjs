import { execFile as execFileCallback } from "node:child_process";
import path from "node:path";
import { promisify } from "node:util";
import { compilerStackConfig } from "./compiler-stack.config.mjs";

export const FINGERPRINT_PATH = "scripts/compiler-stack.fingerprint.json";

// Every coordinated package reachable from the top-level rolldown package in the
// feature-resolved, locked graph, as a sorted (name, version, source) set: the
// oxc*/rolldown* families, plus the glob matcher, which is coordinated for a
// different reason -- the daemon calls it DIRECTLY to answer the Side-Effectful
// badge, and its whole job is to agree with the copy rolldown resolved. Recording
// the version rolldown got is what turns a divergence from our own pin into a red
// test instead of two matchers quietly disagreeing about one `sideEffects` array.
//
// Generated data: `deps:update:compiler` rewrites it, tests recompute it, and
// nobody edits it by hand.
const COORDINATED_PACKAGE = new RegExp(
  `^(?:oxc|rolldown|${compilerStackConfig.globMatcherCrate})`,
  "u",
);

export const fingerprintFromMetadata = (metadata, rootCrateName = "rolldown") => {
  const packagesById = new Map(metadata.packages.map((pkg) => [pkg.id, pkg]));
  const nodesById = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
  const root = metadata.packages.find((pkg) => pkg.name === rootCrateName);
  if (!root) {
    throw new Error(`${rootCrateName} is not present in the resolved graph`);
  }

  const seen = new Set();
  const queue = [root.id];
  const tuples = [];
  while (queue.length > 0) {
    const id = queue.shift();
    if (seen.has(id)) {
      continue;
    }
    seen.add(id);
    const pkg = packagesById.get(id);
    if (!pkg) {
      continue;
    }
    if (COORDINATED_PACKAGE.test(pkg.name)) {
      tuples.push({ name: pkg.name, version: pkg.version, source: pkg.source ?? "path" });
    }
    for (const dependency of nodesById.get(id)?.dependencies ?? []) {
      queue.push(dependency);
    }
  }

  tuples.sort((left, right) =>
    left.name === right.name
      ? left.version.localeCompare(right.version)
      : left.name.localeCompare(right.name),
  );
  return { packages: tuples };
};

export const computeCompilerStackFingerprint = async ({
  execFile = promisify(execFileCallback),
  rootDir = process.cwd(),
  rootCrateName = "rolldown",
} = {}) => {
  const { stdout } = await execFile(
    "cargo",
    [
      "metadata",
      "--locked",
      "--format-version",
      "1",
      "--manifest-path",
      path.join(rootDir, "daemon/Cargo.toml"),
    ],
    // cargo metadata for the full graph is tens of MB; the default 1 MiB
    // maxBuffer truncates it into a parse error.
    { maxBuffer: 256 * 1024 * 1024 },
  );
  return fingerprintFromMetadata(JSON.parse(stdout), rootCrateName);
};

export const formatFingerprint = (fingerprint) => `${JSON.stringify(fingerprint, null, 2)}\n`;
