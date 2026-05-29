# Release Checklist

Use this checklist before publishing any ImportLens VSIX.

## Prerequisites

- `package.json` version matches the GitHub Actions release input.
- `media/icon.png` exists and is the final marketplace icon.
- `pnpm install --frozen-lockfile` succeeds from a clean checkout.
- Windows packaging is verified first because Windows is the primary supported platform.

## Local Verification

Run these commands from the repository root:

```powershell
pnpm check
pnpm test
pnpm test:performance
cargo fmt --check
pnpm package:win32-x64
pnpm docker:build
```

If `pnpm coverage:rust` is run locally, install the pinned coverage tool first:

```powershell
cargo install cargo-llvm-cov --version 0.8.7 --locked
pnpm coverage:rust
```

## CI Gates

- Validate job passes TypeScript, Rust formatting, tests, performance smoke, and Rust line coverage.
- Windows packaging job produces `win32-x64` and `win32-arm64` VSIX artifacts on `windows-latest`.
- Docker packaging job produces Linux and macOS VSIX artifacts from one Ubuntu runner.
- Every VSIX passes the 20 MB size gate.
- Daemon hashes are regenerated during packaging and match the packaged binary.

## Publish Gate

Do not publish if any target is missing, any VSIX exceeds 20 MB, the daemon hash file was not refreshed by the package step, or the release icon is still a placeholder/missing file.
