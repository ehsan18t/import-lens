import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { targetInfo } from "./targets.mjs";

const generatedObjectPattern =
  /export\s+const\s+knownDaemonHashes:\s+Readonly<Record<string,\s+string>>\s+=\s+(\{[\s\S]*?\})\s*;\s*$/u;

export const parseKnownHashesSource = (source) => {
  const trimmed = source.trim();

  if (trimmed === "") {
    return {};
  }

  const match = generatedObjectPattern.exec(source);

  if (!match?.[1]) {
    throw new Error("known daemon hash source does not match the generated format");
  }

  const parsed = JSON.parse(match[1]);

  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("known daemon hash source must contain an object");
  }

  return Object.fromEntries(
    Object.entries(parsed).filter(
      (entry) => typeof entry[0] === "string" && typeof entry[1] === "string",
    ),
  );
};

export const knownHashesSource = (hashes) => {
  const entries = Object.entries(hashes).sort(([left], [right]) => left.localeCompare(right));
  const lines = entries.map(([relativePath, digest], index) => {
    const comma = index === entries.length - 1 ? "" : ",";
    return `  ${JSON.stringify(relativePath)}: ${JSON.stringify(digest)}${comma}`;
  });

  return [
    "export const knownDaemonHashes: Readonly<Record<string, string>> = {",
    ...lines,
    "};",
    "",
  ].join("\n");
};

export const relativeDaemonPath = (target) => {
  const { binaryName } = targetInfo(target);

  return `bin/${target}/${binaryName}`;
};

export const collectDaemonHashes = ({ repoRoot, selectedTargets }) => {
  const hashes = {};

  for (const target of selectedTargets) {
    const relativePath = relativeDaemonPath(target);
    const absolutePath = join(repoRoot, relativePath);

    if (!existsSync(absolutePath)) {
      continue;
    }

    const digest = createHash("sha256").update(readFileSync(absolutePath)).digest("hex");
    hashes[relativePath] = digest;
  }

  return hashes;
};

export const updateKnownDaemonHashes = ({ repoRoot, selectedTargets, existingSource = "" }) => {
  const existingHashes = parseKnownHashesSource(existingSource);
  const nextHashes = { ...existingHashes };

  for (const target of selectedTargets) {
    delete nextHashes[relativeDaemonPath(target)];
  }

  Object.assign(nextHashes, collectDaemonHashes({ repoRoot, selectedTargets }));

  return {
    hashes: nextHashes,
    source: knownHashesSource(nextHashes),
  };
};
