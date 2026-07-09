# ImportLens — Copilot Instructions

> **Read [ImportLens-SRS.md](../docs/ImportLens-SRS.md) first.** It is the authoritative source of truth for all requirements, version pins, and architectural decisions. These instructions supplement it with agent-specific guidance.

## Project Overview

ImportLens is a VS Code extension that shows real-time bundle-size analysis for JavaScript/TypeScript imports. It has two components:

1. **TypeScript extension host** — parses imports, resolves versions, renders UI, communicates over IPC.
2. **Rust daemon** — performs tree-shaking, minification, multi-format compression, and caching via IPC.

## Reference Versions & Pinning Policy

These are current reference versions, **not** a mandate to pin everything. The project follows a tiered dependency-version policy (SRS §9) — stay current automatically wherever it is safe, chosen by the blast radius of an automatic upgrade:

- **Tier 1 — track minor+patch (caret `^`)** where no in-major upgrade can break us (e.g. `redb ^4`, most well-behaved libs, dev tooling like Biome/lefthook).
- **Tier 2 — patch-only (tilde `~`)** where a minor could break: the coordinated OXC stack (`oxc_parser ~0.138.0`, all monorepo crates on ONE version) and `oxc_resolver ~11.22.0`, plus `papaya ~0.2`.
- **Tier 3 — exact (`=`)** only when even a patch can break (e.g. GitHub Action pins, for supply-chain safety).

A caret/tilde range is the intended policy — **do not flag it as an error**.

| Dependency            | Version    | Tier / Notes                                    |
| --------------------- | ---------- | ----------------------------------------------- |
| `oxc_parser` (Rust)   | `~0.138.0` | Patch-pin; all OXC monorepo crates one version. |
| `oxc_resolver` (Rust) | `~11.22.0` | Patch-pin; independent repo/version.            |
| `redb` (Rust)         | `^4`       | Track minor+patch. v4.0.0 minimum (NOT v3).     |
| `papaya` (Rust)       | `~0.2`     | Patch-pin; lock-free, requires pin API.         |
| Node.js (build)       | `24 LTS`   | Build/test/package only.                        |
| `pnpm`                | `11.10.0`  | Pinned through Corepack and CI.                 |
| `@types/vscode`       | `1.90.0`   | Matches `engines.vscode` baseline.              |
| `typescript`          | `6.0.3`    | TS 6.x, NOT 5.x.                                |
| `tsdown`              | `0.22.3`   | Rolldown-powered bundler.                       |
| `@vscode/vsce`        | `3.9.2`    | VSIX package/publish tooling.                   |

## Banned Packages — DO NOT USE

- `@oxc-parser/wasm` and `oxc-parser` (npm) → parse in the Rust daemon (`oxc_parser` crate); the extension host does not parse
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
6. **OXC crate versions**: all monorepo crates share one coordinated version (currently `0.138.0`, patch-pinned `~`); `oxc_resolver` is separate (`~11.22.0`).
7. **redb version**: v4.x, not v3.x. Must have schema versioning (FR-026a).
8. **File watcher glob**: `**/node_modules/*/package.json` (single star), not double star.
9. **sideEffects array**: Treat `["*.css"]` conservatively as `true` in v1.0.
10. **Binary integrity**: SHA-256 verification before spawning daemon (NFR-014a).

## Architecture Quick Reference

```
User types → 300ms debounce → BatchRequest with document source (MessagePack + length-prefix)
→ Unix socket / Named pipe → Rust daemon
→ oxc_parser extracts imports → resolve installed versions from node_modules
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
| Future WASM target (deferred v1.1)  | `vscode-wasm-threads-target` |
| Binary size optimization            | `rust-binary-optimization`   |
| CI/CD, VSIX packaging               | `ci-cross-compilation`       |
