---
name: ci-cross-compilation
description: "CI/CD pipeline for cross-compiling Rust to 6 native targets, building platform-specific VSIXs under 20 MB, and running integration tests. Use when setting up GitHub Actions or CI configuration."
---

# Instructions

ImportLens distributes platform-specific VSIXs. Each contains only the binary for that platform's target.

## 1. Target Matrix

| VSIX Platform  | Rust Target                 | Binary Name              |
| -------------- | --------------------------- | ------------------------ |
| `linux-x64`    | `x86_64-unknown-linux-gnu`  | `import-lens-daemon`     |
| `linux-arm64`  | `aarch64-unknown-linux-gnu` | `import-lens-daemon`     |
| `darwin-x64`   | `x86_64-apple-darwin`       | `import-lens-daemon`     |
| `darwin-arm64` | `aarch64-apple-darwin`      | `import-lens-daemon`     |
| `win32-x64`    | `x86_64-pc-windows-msvc`    | `import-lens-daemon.exe` |
| `win32-arm64`  | `aarch64-pc-windows-msvc`   | `import-lens-daemon.exe` |

## 2. Release Profile (Cargo.toml)

These profiles are mandatory for size compliance:

```toml
[profile.release]
opt-level = "z"        # Optimize for size
codegen-units = 1      # Better LTO
lto = true             # Link-Time Optimization
panic = "abort"        # No unwinding overhead
strip = true           # Strip debug symbols
```

## 3. Native Compilation

For each native target:

```bash
# Cross-compile for a specific target
cargo build --release --target <rust-target> -p import-lens-daemon

# Copy binary to the correct bin/ directory
cp target/<rust-target>/release/import-lens-daemon bin/<vsix-platform>/
```

Use `cross` or platform-specific CI runners for cross-compilation.

## 4. Deferred WASM Fallback

Do not add WASM to v1.0 CI or VSIX packaging. The candidate target is deferred
to v1.1 and requires an SRS update plus end-to-end worker/runtime tests before
it can become a release artifact.

## 5. Binary Hash Generation (NFR-014a)

After compilation, generate SHA-256 hashes for integrity verification:

```bash
sha256sum bin/<platform>/import-lens-daemon > bin/<platform>/sha256
```

These hashes are embedded in the VSIX and checked by the extension host before spawning the daemon.

## 6. VSIX Packaging

Build each platform VSIX using `@vscode/vsce`:

```bash
# IMPORTANT: Use --no-dependencies to exclude devDependencies
npx @vscode/vsce package --target <vsix-platform> --no-dependencies -o import-lens-<platform>.vsix
```

## 7. Size Gate (NFR-007, AC-001)

The CI pipeline MUST fail if any VSIX exceeds 20 MB:

```bash
for vsix in import-lens-*.vsix; do
    size=$(stat -f%z "$vsix" 2>/dev/null || stat -c%s "$vsix")
    if [ "$size" -gt 20971520 ]; then
        echo "FAIL: $vsix exceeds 20 MB ($size bytes)"
        exit 1
    fi
done
```

Expected sizes per platform VSIX: 10–13 MB compressed.

## 8. Integration Tests

Must pass BEFORE any VSIX is published:

- `lodash_es.test.ts` — named exports, tree-shaking accuracy
- `date_fns.test.ts` — large named export surface
- `zod.test.ts` — single entry point, namespace
- `react.test.ts` — default export, CJS detection
- `uuid.test.ts` — small package baseline

## 9. Publish Checklist

1. All integration tests pass.
2. All 5 acceptance criteria (AC-001 through AC-005) verified.
3. All platform VSIXs under 20 MB.
4. All platform VSIXs built in the SAME CI run for version consistency.

## Rules

- Never publish a VSIX with a minifier version that fails the integration suite (C-001 fallback strategy).
- linux-armhf (`armv7-unknown-linux-gnueabihf`) is deferred to v1.1.
- WASM fallback is deferred to v1.1 and must not be added to CI without an SRS update.
