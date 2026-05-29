import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = dirname(scriptDir);
const targets = [
  ["win32-x64", "import-lens-daemon.exe"],
  ["win32-arm64", "import-lens-daemon.exe"],
  ["linux-x64", "import-lens-daemon"],
  ["linux-arm64", "import-lens-daemon"],
  ["darwin-x64", "import-lens-daemon"],
  ["darwin-arm64", "import-lens-daemon"],
];

const hashes = {};

for (const [target, binary] of targets) {
  const relativePath = `bin/${target}/${binary}`;
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

