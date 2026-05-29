import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { platformTargets, targetInfo } from "./targets.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = dirname(scriptDir);
const requestedTargets = process.argv.slice(2);
const selectedTargets = requestedTargets.length > 0 ? requestedTargets : platformTargets;

const hashes = {};

for (const target of selectedTargets) {
  const { binaryName } = targetInfo(target);
  const relativePath = `bin/${target}/${binaryName}`;
  const absolutePath = join(repoRoot, relativePath);

  if (!existsSync(absolutePath)) {
    continue;
  }

  const digest = createHash("sha256").update(readFileSync(absolutePath)).digest("hex");
  hashes[relativePath] = digest;
}

const outputPath = join(repoRoot, "extension/src/daemon/knownHashes.generated.ts");
mkdirSync(dirname(outputPath), { recursive: true });
writeFileSync(
  outputPath,
  `export const knownDaemonHashes: Readonly<Record<string, string>> = ${JSON.stringify(hashes, null, 2)};\n`,
  "utf8",
);

console.log(`Wrote ${Object.keys(hashes).length} daemon hash entr${Object.keys(hashes).length === 1 ? "y" : "ies"} to ${outputPath}`);
