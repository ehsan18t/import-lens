import { execFile as execFileCallback } from "node:child_process";
import path from "node:path";
import { promisify } from "node:util";

export const FINGERPRINT_PATH = "scripts/compiler-stack.fingerprint.json";

// The fingerprint is the sorted (name, version, source) set of every oxc*/
// rolldown* package reachable from the top-level rolldown package in the
// feature-resolved, locked graph. It is generated data: `deps:update:compiler`
// rewrites it, tests recompute it, and nobody edits it by hand.
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
    if (/^(?:oxc|rolldown)/u.test(pkg.name)) {
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
  candidateFeature = "rolldown-candidate",
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
      "--features",
      candidateFeature,
    ],
    // cargo metadata for the full graph is tens of MB; the default 1 MiB
    // maxBuffer truncates it into a parse error.
    { maxBuffer: 256 * 1024 * 1024 },
  );
  return fingerprintFromMetadata(JSON.parse(stdout), rootCrateName);
};

export const formatFingerprint = (fingerprint) => `${JSON.stringify(fingerprint, null, 2)}\n`;
