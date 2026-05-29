---
name: rust-binary-optimization
description: "Cargo release profile settings, LTO, strip, panic=abort, and wasm-opt for achieving sub-20 MB VSIX size. Use when tuning binary size or debugging size regressions (NFR-007)."
---

# Instructions

The VSIX for each platform must stay under 20 MB. The Rust binary is the primary size contributor. These optimizations are mandatory.

## 1. Release Profile

```toml
[profile.release]
opt-level = "z"        # Optimize for SIZE, not speed
codegen-units = 1      # Single codegen unit enables better LTO
lto = true             # Full link-time optimization — critical for size
panic = "abort"        # Removes unwinding tables (~200-500 KB saving)
strip = true           # Strips all debug symbols and DWARF info
```

### Why These Specific Values

| Setting         | Value     | Effect                                                                                                                                              |
| --------------- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `opt-level`     | `"z"`     | Aggressively optimizes for binary size. Use `"z"` not `"s"` — the performance difference is negligible for ImportLens's computation profile.        |
| `codegen-units` | `1`       | Forces all code into one LLVM module, enabling cross-module inlining and dead code elimination. Build time increases ~2x but binary shrinks 10-20%. |
| `lto`           | `true`    | Full (fat) LTO. Enables cross-crate inlining. For thin LTO, use `lto = "thin"` as a fallback if compile times are excessive.                        |
| `panic`         | `"abort"` | Removes the entire unwinding machinery. Safe because the daemon exits on unrecoverable errors anyway.                                               |
| `strip`         | `true`    | Strips debug symbols AND symbol table. Use `strip = "symbols"` if you need to keep the symbol table for profiling.                                  |

## 2. WASM-Specific Profile

```toml
[profile.release-wasm]
inherits = "release"
opt-level = "z"
lto = true
strip = "symbols"
```

After Cargo compilation, apply `wasm-opt` from the Binaryen toolchain:

```bash
wasm-opt -Oz -o output.wasm input.wasm
```

`wasm-opt -Oz` typically reduces WASM binary size by 15-30% through:

- Dead code elimination
- Constant folding
- Stack-based IR optimization

## 3. Dependency Auditing for Size

If the binary exceeds the size budget, use `cargo bloat` to identify the largest contributors:

```bash
cargo install cargo-bloat
cargo bloat --release --target <target> -n 20
```

Common size offenders to watch:

- `regex` — pulls in Unicode tables (~1 MB). Prefer `regex-lite` if full Unicode isn't needed.
- `tokio` — ensure only needed features are enabled (`rt-multi-thread`, `net`, `io-util`, `macros`).
- `serde` — the `derive` feature adds code per struct; this is acceptable.

## 4. Expected Sizes (Reference)

| Component                           | Expected Size |
| ----------------------------------- | ------------- |
| Native daemon (release, stripped)   | 5–10 MB       |
| WASM daemon (wasm-opt -Oz)          | 3–7 MB        |
| TypeScript bundle (tsdown minified) | < 500 KB      |
| Total VSIX (compressed)             | 10–13 MB      |

## Rules

- Never use `opt-level = 3` or `opt-level = 2` for release builds — they optimize for speed at the cost of size.
- Never disable LTO — it's the single most impactful size optimization.
- The 20 MB VSIX limit is a hard CI gate. Size regressions block publishing.
