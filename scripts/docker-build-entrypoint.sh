#!/usr/bin/env bash
set -euo pipefail

echo "Building inside Docker container..."

pnpm install --frozen-lockfile
pnpm check
pnpm test
pnpm test:performance

# zig handles the unix targets; it cannot emit the MSVC ABI, so Windows takes the
# cargo-xwin path. Which target needs which compiler is decided in targets.mjs,
# so adding a target never touches this file.
#
# Both lists are captured into variables, NOT read from a process substitution:
# `set -e` does not observe a process substitution's exit status, so a failing
# generator would build nothing and then hand assert:vsix-size zero arguments --
# whereupon it scans dist/vsix/ and happily passes on VSIXes left over from a
# previous run in the mounted repo.
build_plan="$(node scripts/print-targets.mjs --build-plan)"
vsix_list="$(node scripts/print-targets.mjs --vsix)"

if [ -z "$build_plan" ] || [ -z "$vsix_list" ]; then
  echo "print-targets.mjs produced an empty target list." >&2
  exit 1
fi

while read -r target cross_compiler_flag; do
  node scripts/package-target.mjs "$target" "$cross_compiler_flag"
done <<< "$build_plan"

mapfile -t vsix_files <<< "$vsix_list"
pnpm assert:vsix-size "${vsix_files[@]}"

echo "Docker build completed successfully."
