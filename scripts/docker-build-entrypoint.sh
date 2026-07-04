#!/usr/bin/env bash
set -euo pipefail

echo "Building inside Docker container..."

pnpm install --frozen-lockfile
pnpm check
pnpm test
pnpm test:performance

for target in linux-x64 linux-arm64 darwin-x64 darwin-arm64; do
  node scripts/package-target.mjs "$target" --zigbuild
done

version=$(node -p "JSON.parse(require('node:fs').readFileSync('package.json', 'utf8')).version")
pnpm assert:vsix-size \
  "dist/vsix/import-lens-linux-x64-${version}.vsix" \
  "dist/vsix/import-lens-linux-arm64-${version}.vsix" \
  "dist/vsix/import-lens-darwin-x64-${version}.vsix" \
  "dist/vsix/import-lens-darwin-arm64-${version}.vsix"

echo "Docker build completed successfully."
