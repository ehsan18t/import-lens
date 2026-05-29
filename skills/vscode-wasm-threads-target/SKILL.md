---
name: vscode-wasm-threads-target
description: "Deferred v1.1 WASM fallback research for wasm32-wasip1-threads, wasm-opt optimization, and VS Code Worker execution limits. VS Code Desktop only - NOT VS Code for the Web."
---

# Instructions

The v1.0 release is native-daemon only. Use this skill only when intentionally
implementing the deferred v1.1 WASM fallback after updating the SRS and release
pipeline requirements.

## 1. Rust Target Configuration

The candidate daemon target is `wasm32-wasip1-threads`.

```bash
RUSTFLAGS="-C link-arg=--max-memory=4294967296" cargo build --target wasm32-wasip1-threads --release
wasm-opt -Oz -o import-lens-daemon.wasm target/wasm32-wasip1-threads/release/import-lens-daemon.wasm
```

## 2. Rayon and Memory Constraints (C-004)

Rayon requires multiple threads, which WASM implements using `SharedArrayBuffer`. Because thread stacks consume contiguous memory in WASM bounds:

- We must limit the Rayon thread pool to 1 concurrent task.
- We must pass the linker flag `--max-memory` to prevent out-of-bounds stack exhaustions during heavy AST parsings.

## 3. Strict Execution Location Restrictions

This `.wasm` binary can **only** be executed via the `@vscode/wasm-wasi-core` extension on **VS Code Desktop**.

It **will not work** in standard VS Code for the Web (e.g. github.dev) because native web browser execution currently cannot guarantee cross-origin isolation headers (`COOP`/`COEP`) which are strictly required to spawn threads and execute `SharedArrayBuffer`.

When triggered in the web, the TypeScript daemon must instantly fall back to degraded mode, omitting sizes silently. Do NOT log a crash - this behavior is expected.
