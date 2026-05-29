---
name: vscode-wasm-threads-target
description: "WASM compilation targeting wasm32-wasip1-threads, wasm-opt optimization, and VS Code Worker execution limits. VS Code Desktop only — NOT VS Code for the Web. Use when implementing the WASM fallback tier (C-004)."
---

# Instructions

To run smoothly on VS Code Desktop when a native binary match is missing, we must invoke our daemon as a WebAssembly module (Tier 2).

## 1. Rust Target Configuration

You MUST compile the Rust daemon targeting `wasm32-wasip1-threads`.

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

When triggered in the web, the TypeScript daemon must instantly fall back to Tier 3 (Degraded Mode), omitting sizes silently. Do NOT log a crash — this behavior is expected.
