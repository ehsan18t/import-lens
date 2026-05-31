---
name: project-scaffolding
description: "Complete project file structure, package.json, Cargo.toml workspace, tsconfig.json, and tsdown.config.ts for ImportLens. Use when initializing the project or creating new files."
user-invocable: true
---

# Instructions

This skill defines the authoritative file structure and configuration files for ImportLens. Use this when scaffolding the project from scratch or verifying the structure is correct.

## 1. Directory Structure (§14 of SRS)

```
import-lens/
├── package.json
├── tsconfig.json
├── tsdown.config.ts
├── Cargo.toml                         # Workspace root
├── Cargo.lock
│
├── extension/                         # TypeScript extension host
│   ├── src/
│   │   ├── extension.ts               # activate() / deactivate()
│   │   ├── listener.ts                # onDidChangeTextDocument, debounce
│   │   ├── parser.ts                  # oxc-parser NAPI import extraction
│   │   ├── resolver.ts                # package.json version resolution
│   │   ├── ipc/
│   │   │   ├── client.ts              # Socket/pipe connection management
│   │   │   ├── protocol.ts            # BatchRequest / BatchResponse / Hello / CacheInvalidate / Shutdown types
│   │   │   └── codec.ts               # MessagePack encode/decode with length-prefix framing
│   │   ├── watcher.ts                 # FileSystemWatcher → CacheInvalidate IPC
│   │   ├── ui/
│   │   │   ├── decorations.ts         # End-of-line text decorations
│   │   │   ├── inlayHints.ts          # InlayHintsProvider (kind=undefined, with tooltip)
│   │   │   ├── codelens.ts            # Code lens provider
│   │   │   ├── statusbar.ts           # Status bar item
│   │   │   └── report.ts             # Show Report webview
│   │   ├── logger.ts                  # OutputChannel-based diagnostic logger
│   │   └── config.ts                  # VS Code settings access
│   └── dist/
│       └── extension.js               # tsdown bundle output
│
├── daemon/                            # Rust daemon crate
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                    # Entry point, socket server, Tokio runtime
│       ├── ipc/
│       │   ├── mod.rs
│       │   ├── server.rs              # Unix socket / named pipe listener
│       │   └── protocol.rs            # All IPC message serde types
│       ├── pipeline/
│       │   ├── mod.rs
│       │   ├── resolve.rs             # oxc_resolver usage
│       │   ├── graph.rs               # Module graph walker
│       │   ├── treeshake.rs           # Reachability analysis
│       │   ├── transform.rs           # oxc_transformer (TS/JSX stripping)
│       │   ├── minify.rs              # oxc_minifier + oxc_mangler
│       │   ├── codegen.rs             # oxc_codegen AST-to-string
│       │   └── compress.rs            # flate2 + brotli + zstd (nested rayon::join)
│       ├── cache/
│       │   ├── mod.rs
│       │   ├── memory.rs              # papaya HashMap (pinning API)
│       │   └── persistent.rs          # redb v4 read/write with schema versioning
│       ├── lifecycle.rs               # Graceful shutdown, self-recycle (NFR-004a)
│       └── prefetch.rs               # Background pre-warm logic
│
├── bin/                               # Native daemon binaries (gitignored, CI-populated)
│   ├── linux-x64/import-lens-daemon
│   ├── linux-arm64/import-lens-daemon
│   ├── darwin-x64/import-lens-daemon
│   ├── darwin-arm64/import-lens-daemon
│   ├── win32-x64/import-lens-daemon.exe
│   └── win32-arm64/import-lens-daemon.exe
│
└── tests/
    ├── fixtures/packages/
    └── integration/
        ├── lodash_es.test.ts
        ├── date_fns.test.ts
        ├── zod.test.ts
        ├── react.test.ts
        └── uuid.test.ts
```

## 2. Root Cargo.toml (Workspace)

```toml
[workspace]
members = ["daemon"]
resolver = "2"

[workspace.package]
edition = "2024"
rust-version = "1.89.0"
```

## 3. Daemon Cargo.toml — Pinned Versions

All OXC crates MUST be pinned to `~0.133`. `oxc_resolver` is independent at `~11.19`.

```toml
[package]
name = "import-lens-daemon"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
# OXC suite — ALL must be same version
oxc_parser = "~0.133"
oxc_resolver = "~11.19"
oxc_semantic = "~0.133"
oxc_transformer = "~0.133"
oxc_minifier = "~0.133"
oxc_mangler = "~0.133"
oxc_codegen = "~0.133"
oxc_allocator = "~0.133"
oxc_span = "~0.133"

# Caching
papaya = "~0.2"
redb = "^4"

# Concurrency
rayon = "^1.12"
tokio = { version = "^1.52", features = ["rt-multi-thread", "net", "io-util", "macros"] }

# Serialization
rmp-serde = "^1.3"
serde = { version = "^1", features = ["derive"] }

# Compression
flate2 = "^1.1"
brotli = "^8"
zstd = "~0.13"

[profile.release]
opt-level = "z"
codegen-units = 1
lto = true
panic = "abort"
strip = true

```

## 4. package.json — Key Fields

```json
{
  "name": "import-lens",
  "displayName": "ImportLens",
  "publisher": "<your-publisher-id>",
  "engines": {
    "vscode": "^1.100.0"
  },
  "main": "./extension/dist/extension.cjs",
  "activationEvents": [
    "onLanguage:javascript",
    "onLanguage:typescript",
    "onLanguage:typescriptreact",
    "onLanguage:javascriptreact"
  ],
  "dependencies": {
    "oxc-parser": "0.133.0",
    "@msgpack/msgpack": "3.1.3"
  },
  "devDependencies": {
    "tsdown": "0.22.1",
    "typescript": "6.0.3",
    "@types/vscode": "1.100.0",
    "@vscode/vsce": "3.9.1"
  }
}
```

> [!IMPORTANT]
>
> - `oxc-parser` is v0.133.0, NOT v0.123.0. The npm package MUST match the Rust crate version.
> - `@types/vscode` MUST be 1.100.0, matching the minimum `engines.vscode`.
> - `@vscode/vsce` MUST be 3.9.1. Use `--no-dependencies` when building VSIX.
> - Do NOT use `@oxc-parser/wasm`, `dashmap`, `sled`, or `num_cpus` anywhere.

## 5. Banned Packages (§9.4.4)

These must NEVER appear in `Cargo.toml` or `package.json`:

- `@oxc-parser/wasm` (npm) → use `oxc-parser` NAPI
- `sled` (Rust) → use `redb` v4
- `dashmap` (Rust) → use `papaya`
- `num_cpus` (Rust) → use `std::thread::available_parallelism()`

## Rules

- The `bin/` directory is gitignored and CI-populated.
- The extension must be bundled to a single file by `tsdown`. Only `oxc-parser` (NAPI) and `@msgpack/msgpack` are runtime dependencies.
- The `vscode` module must always be marked as `external` in tsdown config.
