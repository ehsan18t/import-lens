import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { updateKnownDaemonHashes } from "./daemon-hashes.mjs";
import { platformTargets } from "./targets.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = dirname(scriptDir);
const requestedTargets = process.argv.slice(2);
const selectedTargets = requestedTargets.length > 0 ? requestedTargets : platformTargets;
const outputPath = join(repoRoot, "extension/src/daemon/knownHashes.generated.ts");
const existingSource = existsSync(outputPath) ? readFileSync(outputPath, "utf8") : "";

try {
  const { hashes, source } = updateKnownDaemonHashes({
    repoRoot,
    selectedTargets,
    existingSource,
  });

  mkdirSync(dirname(outputPath), { recursive: true });
  writeFileSync(outputPath, source, "utf8");

  const count = Object.keys(hashes).length;
  console.log(`Wrote ${count} daemon hash entr${count === 1 ? "y" : "ies"} to ${outputPath}`);
} catch (error) {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
}
