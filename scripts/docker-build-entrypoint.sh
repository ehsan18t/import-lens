#!/usr/bin/env bash
set -euo pipefail

echo "Building inside Docker container..."

pnpm install --frozen-lockfile
pnpm check
pnpm test:scripts
pnpm test:rust

for target in linux-x64 linux-arm64 darwin-x64 darwin-arm64; do
  node scripts/build-daemon.mjs "$target" --zigbuild
  pnpm run "copy:daemon:$target"
done

for target in linux-x64 linux-arm64 darwin-x64 darwin-arm64; do
  node scripts/generate-daemon-hashes.mjs "$target"
  pnpm build
  node scripts/package-vsix.mjs "$target"
done

version=$(node -p "JSON.parse(require('node:fs').readFileSync('package.json', 'utf8')).version")
pnpm assert:vsix-size \
  "import-lens-linux-x64-${version}.vsix" \
  "import-lens-linux-arm64-${version}.vsix" \
  "import-lens-darwin-x64-${version}.vsix" \
  "import-lens-darwin-arm64-${version}.vsix"

echo "Docker build completed successfully."
