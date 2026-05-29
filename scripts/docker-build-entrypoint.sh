#!/bin/bash
set -e

echo "Building inside Docker container..."

# We need to compile the daemon for the specified targets
cargo zigbuild -p import-lens-daemon --target x86_64-unknown-linux-gnu --release
cargo zigbuild -p import-lens-daemon --target aarch64-unknown-linux-gnu --release
cargo zigbuild -p import-lens-daemon --target x86_64-apple-darwin --release
cargo zigbuild -p import-lens-daemon --target aarch64-apple-darwin --release

# Run pnpm install to prepare extension packaging
pnpm install

# Run the package scripts
pnpm package:linux-x64
pnpm package:linux-arm64
pnpm package:darwin-x64
pnpm package:darwin-arm64

echo "Docker build completed successfully."
