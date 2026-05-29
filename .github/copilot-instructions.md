# ImportLens — Copilot Instructions

> **Read [ImportLens-SRS.md](../docs/ImportLens-SRS.md) first.** It is the authoritative source of truth for all requirements, version pins, and architectural decisions. These instructions supplement it with agent-specific guidance.

## Project Overview

ImportLens is a VS Code extension that shows real-time bundle-size analysis for JavaScript/TypeScript imports. It has two components:

1. **TypeScript extension host** — parses imports, resolves versions, renders UI, communicates over IPC.
2. **Rust daemon** — performs tree-shaking, minification, multi-format compression, and caching via IPC.

## Critical Version Pins

Before writing ANY code, verify these versions. They are the most common source of agent errors:

| Dependency            | Pinned Version | Notes                                  |
| --------------------- | -------------- | -------------------------------------- |
| `oxc-parser` (npm)    | `0.133.0`      | NOT 0.123.0. Must match Rust crate.    |
| `oxc_parser` (Rust)   | `~0.133`       | All OXC crates same version.           |
| `oxc_resolver` (Rust) | `~11.19`       | Independent repo, independent version. |
| `redb` (Rust)         | `^4`           | NOT v3. v4.0.0 minimum.                |
| `papaya` (Rust)       | `~0.2`         | Lock-free, requires pin API.           |
| `@types/vscode`       | `1.100.0`      | Matches `engines.vscode`.              |
| `typescript`          | `6.0.3`        | TS 6.x, NOT 5.x.                       |
| `tsdown`              | `0.22.1`       | Rolldown-powered bundler.              |
| `@vscode/vsce`        | `3.9.1`        | Use `--no-dependencies` flag.          |

## Banned Packages — DO NOT USE

- `@oxc-parser/wasm` → use `oxc-parser` (NAPI)
- `sled` → use `redb` v4
- `dashmap` → use `papaya`
- `num_cpus` → use `std::thread::available_parallelism()`
- `rolldown_core` → unstable, do not use (C-003)
- `notify` (Rust) → VS Code file watcher only (FR-027)

## Common Agent Mistakes to Avoid

1. **InlayHintKind**: Use `undefined`, NOT `InlayHintKind.Parameter` or `InlayHintKind.Type`.
2. **Rayon pool size**: `max(1, available_parallelism - 2)`, NOT 1x logical cores.
3. **Socket path**: Must include window-unique identifier (NFR-014b).
4. **Length-prefix framing**: Every IPC message needs a 4-byte big-endian length header.
5. **Shutdown sequence**: 3 steps — Shutdown IPC → 5s → SIGTERM → 2s → SIGKILL (Unix).
6. **oxc-parser version**: 0.133.0, not 0.123.0.
7. **redb version**: v4.x, not v3.x. Must have schema versioning (FR-026a).
8. **File watcher glob**: `**/node_modules/*/package.json` (single star), not double star.
9. **sideEffects array**: Treat `["*.css"]` conservatively as `true` in v1.0.
10. **Binary integrity**: SHA-256 verification before spawning daemon (NFR-014a).

## Architecture Quick Reference

```
User types → 300ms debounce → oxc-parser (NAPI) → filter imports
→ resolve versions from node_modules → BatchRequest (MessagePack + length-prefix)
→ Unix socket / Named pipe → Rust daemon
→ papaya cache check → [miss: oxc_resolver → module graph → tree-shake
→ oxc_minifier → nested rayon::join(gzip, (brotli, zstd))]
→ BatchResponse → decorations / inlay hints
```

## Skill Index

When implementing a specific area, load the relevant skill:

| Area                                | Skill                        |
| ----------------------------------- | ---------------------------- |
| Project setup, file structure       | `project-scaffolding`        |
| IPC wire protocol, message types    | `ipc-message-protocol`       |
| TypeScript IPC client               | `ts-ipc-client`              |
| Rust IPC server                     | `rust-tokio-ipc-server`      |
| Import parsing (extension host)     | `ts-oxc-parser-napi`         |
| Package version resolution          | `ts-package-resolver`        |
| Document listener, debounce         | `ts-debounce-listener`       |
| Module resolution (daemon)          | `rust-oxc-resolver`          |
| AST pipeline (parse→minify→codegen) | `rust-oxc-pipeline-runner`   |
| Module graph, tree-shaking          | `rust-module-graph-walker`   |
| Compression (gzip/brotli/zstd)      | `rust-compression-pipeline`  |
| Caching (papaya + redb)             | `rust-concurrent-cache`      |
| Daemon lifecycle, self-recycle      | `rust-daemon-lifecycle`      |
| Daemon spawn, shutdown, integrity   | `ts-daemon-lifecycle`        |
| UI (decorations, inlay hints)       | `ts-vscode-ui`               |
| VS Code settings                    | `vscode-extension-settings`  |
| File watcher, logging               | `ts-vscode-workspace`        |
| Build config (TS, tsdown)           | `ts-build-configuration`     |
| WASM target, Worker execution       | `vscode-wasm-threads-target` |
| Binary size optimization            | `rust-binary-optimization`   |
| CI/CD, VSIX packaging               | `ci-cross-compilation`       |
