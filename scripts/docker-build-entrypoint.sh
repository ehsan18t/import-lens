#!/usr/bin/env bash
set -euo pipefail

echo "Building inside Docker container..."

pnpm install --frozen-lockfile
pnpm check
pnpm test
pnpm test:performance

# zig handles the unix targets; the MSVC ABI it cannot emit, so Windows takes
# the cargo-xwin path instead. Together they cover the full six-target set.
for target in linux-x64 linux-arm64 darwin-x64 darwin-arm64; do
  node scripts/package-target.mjs "$target" --zigbuild
done

for target in win32-x64 win32-arm64; do
  node scripts/package-target.mjs "$target" --xwin
done

version=$(node -p "JSON.parse(require('node:fs').readFileSync('package.json', 'utf8')).version")
pnpm assert:vsix-size \
  "dist/vsix/import-lens-linux-x64-${version}.vsix" \
  "dist/vsix/import-lens-linux-arm64-${version}.vsix" \
  "dist/vsix/import-lens-darwin-x64-${version}.vsix" \
  "dist/vsix/import-lens-darwin-arm64-${version}.vsix" \
  "dist/vsix/import-lens-win32-x64-${version}.vsix" \
  "dist/vsix/import-lens-win32-arm64-${version}.vsix"

echo "Docker build completed successfully."
