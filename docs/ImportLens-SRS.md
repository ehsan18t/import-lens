# Software Requirements Specification: Import Lens

**VS Code Import Size Analyzer**

| Field    | Value            |
| -------- | ---------------- |
| Version  | 1.9              |
| Date     | 1 July 2026      |
| Status   | Draft            |
| Audience | Engineering Team |

---

## Table of Contents

1. [Introduction](#1-introduction)
2. [Overall Description](#2-overall-description)
3. [System Architecture](#3-system-architecture)
4. [Architectural Alternatives and Rationale](#4-architectural-alternatives-and-rationale)
5. [Functional Requirements](#5-functional-requirements)
6. [Error Handling and Edge Cases](#6-error-handling-and-edge-cases)
7. [Non-Functional Requirements](#7-non-functional-requirements)
8. [Acceptance Criteria](#8-acceptance-criteria)
9. [Technical Stack](#9-technical-stack)
10. [Component Specifications](#10-component-specifications) (includes §10.7 Module Graph Walk Algorithm)
11. [Data Models](#11-data-models)
12. [Distribution and Packaging](#12-distribution-and-packaging)
13. [Constraints and Assumptions](#13-constraints-and-assumptions)
14. [Appendix A: File Structure](#14-appendix-a-file-structure)
15. [Appendix B: Decision Log](#15-appendix-b-decision-log)
16. [Appendix C: Technology Watch](#16-appendix-c-technology-watch)

---

## 1. Introduction

### 1.1 Purpose

This Software Requirements Specification defines the requirements for Import Lens, a Visual Studio Code extension that calculates and displays the real-world bundle cost of npm package imports directly inside the editor. The document covers functional behaviour, system architecture, technical stack decisions, performance requirements, and distribution constraints.

The primary audience is the engineering team responsible for building and maintaining the extension.

### 1.2 Scope

Import Lens analyses import statements in JavaScript and TypeScript files and shows, inline next to each import, the actual post-tree-shake, minified, and compressed byte size that the import would add to a production bundle. The extension also surfaces bundle-impact insights such as working-tree import deltas, shared dependency explanations, package history trends, and tree-shaking opportunity actions. The extension does this without running the user's build system, without modifying any project files, and without blocking the editor.

The system performs real tree-shaking and minification inside a background Rust daemon process. Results are cached persistently so that repeat lookups are instant. The extension works for any project that uses npm packages, regardless of which bundler the project itself uses.

**Out of scope for v1.0:**

- Local relative imports (e.g. `import { util } from './helpers'`)
- CSS, image, or other non-JS/TS asset imports
- Monorepo cross-package imports where the dependency is not published to npm
- Support for Yarn Plug-n-Play (PnP) without `node_modules`

### 1.3 Definitions and Acronyms

| Term         | Definition                                                                              |
| ------------ | --------------------------------------------------------------------------------------- |
| OXC          | The Oxidation Compiler, a suite of high-performance JS/TS tools written in Rust         |
| VSIX         | Visual Studio Extension package, the distribution unit for VS Code extensions           |
| IPC          | Inter-Process Communication, the channel between the extension host and the Rust daemon |
| MessagePack  | A binary serialization format used as the IPC encoding layer                            |
| Unix socket  | A POSIX IPC endpoint used on macOS and Linux                                            |
| Named pipe   | The Windows equivalent of a Unix socket, used for IPC on Win32 targets                  |
| Tree-shaking | Dead code elimination that retains only the symbols actually used by an import          |
| redb         | An embedded, ACID-compliant key-value database written in pure Rust                     |
| papaya       | A lock-free concurrent hash map crate for Rust                                          |
| WASM         | WebAssembly, a portable binary instruction format                                       |
| WASI         | WebAssembly System Interface, the ABI for running WASM outside a browser                |
| ESM          | ECMAScript Modules, the static module format required for effective tree-shaking        |
| LTO          | Link-Time Optimization, a compiler setting that reduces Rust binary size                |
| SRS          | Software Requirements Specification                                                     |
| FR           | Functional Requirement                                                                  |
| NFR          | Non-Functional Requirement                                                              |
| AST          | Abstract Syntax Tree                                                                    |
| CJS          | CommonJS, the older Node.js module format that does not support static tree-shaking     |

### 1.4 Document Conventions

Requirements are identified with a unique ID of the form `FR-NNN` for functional requirements and `NFR-NNN` for non-functional requirements. Each requirement is a single, testable statement.

Priority levels:
- **Critical:** Must ship in v1.0
- **High:** Targeted for v1.0
- **Medium:** v1.1 candidate

### 1.5 References

- OXC project documentation: https://oxc.rs
- Rolldown bundler: https://rolldown.rs
- redb database: https://github.com/cberner/redb
- papaya crate: https://github.com/ibraheemdev/papaya
- VS Code Extension API: https://code.visualstudio.com/api
- MessagePack specification: https://msgpack.org
- VS Code Platform-Specific Extensions: https://code.visualstudio.com/api/working-with-extensions/publishing-extension

---

## 2. Overall Description

### 2.1 Product Perspective

Import Lens is a standalone VS Code extension. It does not replace or wrap any existing extension. It complements bundler tooling (Vite, webpack, Rolldown, etc.) by surfacing import cost information at authoring time rather than after a build.

Unlike existing calculators that spin up Node.js bundlers, Import Lens offloads all heavy computation to a decoupled Rust background process. This guarantees editor stability and minimal memory overhead inside the extension host. The daemon protocol is kept behind a transport boundary so a future WebAssembly worker can reuse it, but v1.0 ships native daemon binaries only.

The extension introduces a background native process (the Rust daemon) which runs separately from the VS Code extension host. This separation is a deliberate design choice: the extension host is a shared Node.js process that also runs every other installed extension. Placing CPU-intensive work (parsing, tree-shaking, compression) inside the extension host would degrade the entire editor. The daemon runs in its own process with its own memory space, and a crash in the daemon does not affect VS Code. When a supported file is opened outside a VS Code workspace folder, the extension derives an analysis root by walking upward from the file to the nearest `package.json` or `node_modules` directory and still resolves packages from the active document path.

### 2.2 Product Functions

At a high level, Import Lens:

1. Detects import statements in the currently active JS/TS file
2. Filters to node_modules imports only
3. Resolves the installed version of each package from the project's node_modules
4. Sends a batched request to the background Rust daemon over a local socket
5. Receives computed size data (raw, minified, and compressed) for each import
6. Renders the size inline in the editor as confidence-styled inline hints by default, native accessible inlay hints, end-of-line decorations, or code lens annotations
7. Adds contextual insights such as Git working-tree deltas, per-import history trends, shared-byte explanations, and barrel re-export warnings
8. Provides commands for current-file totals, bundle impact history, workspace reports, diagnostic copying, and cache management
9. Provides CodeActions for non-tree-shakeable imports, including named-export candidate enumeration for namespace imports
10. Caches all results so subsequent lookups are instantaneous

### 2.3 User Classes

**Primary user:** A JavaScript or TypeScript developer who imports npm packages and wants to understand the bundle cost of each import without leaving the editor or running a build.

**Secondary user:** A team lead or architect who reviews code with bundle size awareness as part of code review or dependency auditing.

### 2.4 Operating Environment

The extension targets the following environments:

| Tier     | Environment                                                                                  | Mechanism                                                 |
| -------- | -------------------------------------------------------------------------------------------- | --------------------------------------------------------- |
| Native   | VS Code Desktop on win32-x64, win32-arm64, linux-x64, linux-arm64, darwin-x64, darwin-arm64  | Native Rust binary daemon                                 |
| Degraded | VS Code for the Web, unsupported native platforms, or environments without a loadable daemon | Extension-host import detection only, no size computation |

### 2.5 Design and Implementation Constraints

- The extension must not modify any file in the user's workspace.
- The extension must not require the user to install any external tools (no separate CLI install step).
- The Rust daemon must be a self-contained binary with no runtime dependencies on Node.js, Python, or any other interpreter.
- All IPC communication must be local only. No network requests are made as part of size computation.
- The extension host component must be written in TypeScript 7.x and compiled to a single bundled JS file using `tsdown`. The minimum supported VS Code version is 1.90.0, declared via `"engines": { "vscode": "^1.90.0" }` in `package.json`. This version provides a modern baseline while ensuring compatibility with AI-focused VS Code forks (such as Cursor, Windsurf, and Antigravity) that often lag several months behind upstream releases.
- The `tsconfig.json` must use TypeScript 7.x conventions: `module: "esnext"`, an explicit `types` array (not auto-include; currently `["node", "vscode"]`), `moduleResolution: "bundler"`, and `target: "es2025"`. Legacy module formats (`amd`, `umd`, `systemjs`) and legacy `moduleResolution: "node"` (Node10) must not be used.
- The native daemon must be compiled separately for each target platform and distributed as a platform-specific VSIX.
- The published VSIX for any single platform target must not exceed 20 MB.

### 2.6 Assumptions and Dependencies

- The user's project has a `node_modules` directory populated by a package manager (npm, yarn, or pnpm with hoisting).
- Each importable package has a `package.json` in its `node_modules/<package>/` directory. A parseable string `version` field enables exact cache identity; a malformed or versionless manifest is still requestable, but it cannot be measured — the import is reported **Unmeasured** with a `package_manifest` stage (FR-026d). It is not given the package directory's size on disk.
- Packages that expose ESM entry points (via the `exports` or `module` field in `package.json`) will produce accurate tree-shaken sizes. CommonJS-only packages are measured through Rolldown's link-time interop (FR-024a), which retains the whole library — the correct cost, reported with a conservative-sizing warning. A package the engine cannot build at all has **no size**, not an approximate one.

---

## 3. System Architecture

### 3.1 Architectural Overview

The system has three layers: the extension host (TypeScript), the Rust daemon (native binary), and the local cache (in-memory plus persistent).

```
┌────────────────────────────────────────────────────────┐
│                    VS Code Editor                      │
│                                                        │
│  ┌─────────────────────────────────────────────────┐   │
│  │              Extension Host (Node.js)           │   │
│  │                                                 │   │
│  │  ┌────────────────────┐  ┌──────────────────┐   │   │
│  │  │  Document Listener │  │  Decoration      │   │   │
│  │  │  (debounced 300ms) │  │  Renderer        │   │   │
│  │  └────────┬───────────┘  └────────┬─────────┘   │   │
│  │           │ source/path IPC       │ states      │   │
│  │  ┌────────▼───────────────────────▼─────────┐   │   │
│  │  │       IPC Client (MessagePack)           │   │   │
│  │  └────────────────────┬─────────────────────┘   │   │
│  └───────────────────────│─────────────────────────┘   │
└──────────────────────────│─────────────────────────────┘
                           │ Unix socket / Named pipe
┌──────────────────────────▼──────────────────────────────┐
│                  Rust Daemon Process                    │
│                                                         │
│  ┌───────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │  papaya   │  │  OXC         │  │  Compression     │  │
│  │  (in-mem  │  │  Pipeline    │  │  flate2 (gzip)   │  │
│  │   cache)  │  │  parse       │  │  brotli          │  │
│  └─────┬─────┘  │  resolve     │  │  zstd            │  │
│        │        │  semantic    │  └──────────────────┘  │
│  ┌─────▼─────┐  │  tree-shake  │                        │
│  │  redb     │  │  minify      │                        │
│  │ (persist. │  │  mangle      │                        │
│  │   cache)  │  │  codegen     │                        │
│  └───────────┘  └──────────────┘                        │
└─────────────────────────────────────────────────────────┘
```

### 3.2 Deployment Tiers

**Tier 1 - Native (preferred):**
The Rust daemon is compiled to a native binary for the host platform. The extension host communicates with it via a Unix domain socket (macOS/Linux) or a named pipe (Windows), using MessagePack framing. This is the fastest configuration.

**Tier 2 - Degraded:**
If a native binary is unavailable or cannot be verified, or if the environment is VS Code for the Web where local `node_modules` access is unavailable, the extension operates in degraded mode. The UI shows a status bar indicator explaining that full analysis is unavailable.

**Post-v1 Candidate - WASM Desktop Fallback:**
A WebAssembly daemon fallback may be added in v1.1 or later using the existing analysis transport boundary. It is not a v1.0 runtime path and must not be advertised or packaged until the `wasm32-wasip1-threads` build, VS Code Worker execution model, and release pipeline are proven end-to-end. See constraint C-004 in Section 13.1.

### 3.3 Startup Sequence

1. Extension activates on the `onLanguage:javascript`, `onLanguage:typescript`, `onLanguage:typescriptreact`, `onLanguage:javascriptreact`, `onLanguage:json`, `onLanguage:jsonc`, `onLanguage:svelte`, `onLanguage:astro`, and `onLanguage:vue` events.
2. The extension host checks for a native binary matching the current platform in the extension's `dist/bin/` directory.
3. If found, it verifies the binary's SHA-256 hash against the known-good hash embedded in the extension package (NFR-014a). If the hash does not match, the extension logs a security warning and enters degraded mode.
4. If the hash matches, it spawns the daemon process, pipes daemon stdout/stderr into the Import Lens output channel according to the configured log level, opens a socket connection, and sends a versioned `HelloMessage`. The socket path includes a window-unique identifier (NFR-014b).
5. If no native binary is found, or if the binary cannot be verified, spawned, connected, or sent a hello message, the extension disposes any partial IPC client state, terminates the spawned daemon process when it is still alive, and enters the restart/degraded-mode path defined in FR-015.
6. The daemon opens the persistent `redb` cache shard for the active project from the extension-managed VS Code workspace storage cache base, verifies the schema version (FR-026a), and preloads only a bounded set of recent valid size entries into the in-memory `papaya` cache. The daemon must never create cache folders inside the user's project tree.
7. The extension is ready to accept requests.

### 3.4 Request Lifecycle

On each daemon respawn, the extension host reads `<globalStoragePath>/importlens-recycles.json` before deciding whether to spawn, applying the recycle rate limit defined in NFR-004b.


1. The user opens or edits a supported JS/TS, JSX/TSX, Svelte, Astro, or Vue file.
2. The document listener fires after a 300ms debounce.
3. The extension sends an `AnalyzeDocumentRequest` containing the document text, active path, workspace root, configured compression format, and display thresholds.
4. The daemon extracts parseable script regions for component files. Plain JS/TS and JSX/TSX files are parsed as one region; Svelte `<script>` blocks and Vue `<script>` / `<script setup>` blocks are parsed as component regions; Astro frontmatter is parsed as server runtime and processed Astro `<script>` blocks are parsed as client runtime.
5. The daemon parses each script region with Rust `oxc_parser`, extracts ESM import information from module records, maps region-relative ranges back to absolute document positions, and applies `.importlensignore` plus package/specifier filtering.
6. For each remaining import, the daemon resolves the installed package by reading `node_modules/<package>/package.json`. For scoped packages (e.g. `@babel/core`), the path includes the scope directory. If the package directory exists but the manifest is malformed or lacks a string `version`, the daemon uses an unknown-version sentinel so the import is reported **Unmeasured** with a `package_manifest` stage (FR-026d) rather than being marked missing. It is not given an approximate size.
7. The daemon checks its `papaya` map for each import's cache key. Cache hits are answered from the cache and never construct a bundler. A miss is marked `loading` and its build is **deferred past the response** (FR-004a) — the response is never behind an engine build.
8. The daemon serialises one `AnalyzeDocumentResponse` over the socket at once, carrying the cache hits and a `loading` placeholder per miss. Legacy `BatchRequest`/`BatchResponse` remains available for protocol compatibility, but document analysis clients must prefer the daemon-first document endpoint.
9. The deferred misses are drained onto the two-permit async engine boundary (FR-023). For each, the daemon runs the engine pipeline: (a) resolve the package entry point via `oxc_resolver`, (b) build and link the transitive module graph with the embedded Rolldown bundler from a virtual entry (Rolldown owns resolution, ESM/CJS interop, side-effect interpretation, and tree-shaking, and emits one unminified ESM chunk under the Section 10.7 limits), (c) validate the linked chunk with `oxc_semantic`, (d) run `oxc_minifier` for dead code elimination and mangling, (e) emit the minified string via `oxc_codegen` using the minifier-provided scoping and private-member mappings, and (f) compress in parallel with `flate2`, `brotli`, and `zstd` using nested `rayon::join` calls. A build that outlives its own 8s limit delays only its own import (FR-004a).
10. Each completed import is written to `papaya` (memory) and `redb` (disk) — unless a transient engine failure degraded it (FR-026c) — and pushed to the client on the `refreshed_results` channel as it lands. Once the last of them has landed, the daemon re-derives shared-module bytes across the whole document and pushes the corrections, since sharing is only knowable when every import of the file has been measured (FR-004a). Every one of these frames leaves through the same outbound channel as the responses themselves (FR-004c), so no request being served can hold them up.
11. The extension host deserialises responses, discards stale `request_id` values, and updates decorations without regressing newer results. A pushed import is merged into that import's state by identity; a push whose analysis generation has been superseded is dropped.
12. When the response for a document is current, the extension enriches ready states with extension-side insights: Git working-tree import deltas, per-import history trends, shared-module explanations, and barrel re-export warnings. The same insights are re-derived over the merged states when a pushed import lands, so a size that arrived late is captioned like one that arrived on time.
13. The extension records bounded per-import and current-file history entries in VS Code global storage. History persistence failures are logged but must not mark an otherwise successful size result unavailable.

---

## 4. Architectural Alternatives and Rationale

This section documents the key architectural decisions made before implementation and the alternatives that were evaluated. The primary constraint driving all decisions was a hard 20 MB per-platform VSIX size limit.

### 4.1 Bundler and Pipeline Selection

**Evaluated:** Rspack, Rolldown, ESBuild, and OXC (original selection); re-evaluated Rolldown, esbuild, SWC bundler, Rspack, and Farm in the 2026 bundler redesign.

**Original decision (superseded for linking/tree-shaking):** At initial design time, Rspack and Rolldown exposed Node.js APIs rather than embeddable Rust crates, and ESBuild is written in Go. OXC was selected for the full pipeline, with a custom module graph walker for tree-shaking because OXC does not provide a standalone tree-shaker.

**2026 revision — Rolldown adopted for linking and tree-shaking:** Rolldown now publishes an embeddable Rust crate (`rolldown` on crates.io). The custom module graph walker accumulated structural correctness defects (dangling generated bindings, dropped effectful initializers, silently merged ambiguous star exports, empty external re-export bundles), so the bundler-redesign design (`docs/superpowers/specs/2026-07-10-bundler-redesign-design.md`) qualified Rolldown 1.1.5 against a committed construct matrix, pinned real packages, and absolute performance gates. Every gate passed on 2026-07-11 (cold `css-tree/parse` p95 52.4 ms against a 500 ms gate; 20-import batch peak RSS 78 MB against a 400 MB gate; candidate ~1.9x faster than the legacy engine), and Rolldown replaced the custom walker as the only semantic bundler. Two corrections to that record, both from 2026-07-14: the suite carrying those gates was never invoked by CI (a *different*, legacy suite ran under a similar name), and the startup, cache-hit and idle-RSS gates had never been measured at all. Both are fixed — the gates run on every pull request and are now measured against the shipped daemon binary (startup 17 ms, cache hit 15 ms, idle RSS 21 MB) — and no gate was found to be failing. Because Rolldown's Rust API carries no semver guarantee, it is exact-pinned as part of the coordinated compiler stack behind one narrow adapter (see C-003 and §10.7).

**OXC retained for everything around the bundler:** direct OXC crates (`oxc_parser`, `oxc_resolver`, `oxc_semantic`, `oxc_minifier`, `oxc_codegen`) parse the user's document, resolve the root package request, and validate and minify Rolldown's linked output. OXC is the compiler toolchain Rolldown itself is built on.

### 4.2 Minifier Selection

**Evaluated:** SWC Core, Terser, and OXC Minifier.

**Terser rejected:** Terser is a JavaScript tool and would require a Node.js subprocess from within the Rust daemon, contradicting the native-first architecture.

**SWC Core rejected:** SWC produces slightly better compression ratios but its platform-specific binary is approximately 25 to 27 MB depending on the target. Including SWC would push every platform VSIX over the 20 MB hard limit.

**OXC Minifier selected:** It is part of OXC's stable 0.139.x toolchain. The 0.x version number does not indicate alpha quality; it reflects the Rust and npm package versioning scheme used before a 1.0 line. Minified output may vary by 1 to 2 percent from SWC, which is acceptable for a size estimation tool. See Section 13.1 for the upgrade policy.

### 4.3 Extension-Side Parsing

**Evaluated:** Regular expressions, TypeScript Compiler API, and OXC WASM Parser.

**Regular expressions rejected:** They fail on multi-line imports, re-exports, and complex TypeScript syntax patterns.

**TypeScript Compiler API rejected:** It introduces heavy initialization overhead, requires the `typescript` npm package as a runtime dependency, and does not work in VS Code for the Web.

**Rust OXC parser selected:** Import parsing lives in the daemon so VS Code, CLI, and future editors share one implementation. The daemon uses Rust `oxc_parser`, which returns ECMAScript module information in structured module-record arrays without requiring a full extension-host AST walk. The deprecated `@oxc-parser/wasm` package and the Node `oxc-parser` NAPI package are not runtime dependencies.

### 4.4 IPC Encoding

**Evaluated:** JSON, Protocol Buffers, and MessagePack.

**JSON rejected:** JSON is verbose and slower to deserialise. On every debounce cycle the overhead compounds.

**Protocol Buffers rejected:** The schema definition and code generation overhead is disproportionate for a small, well-defined local IPC protocol.

**MessagePack selected:** MessagePack payloads are typically 20-40% smaller than equivalent JSON. In the Rust `rmp-serde` path, deserialization is consistently faster than JSON. In the Node.js extension host, the performance advantage is modest for small payloads but meaningful for batch responses containing 20+ import results. `rmp-serde` on the Rust side integrates directly with `serde` at zero additional cost.

### 4.5 Process Isolation Strategy

**Evaluated:** napi-rs native addon and separate daemon process.

**napi-rs native addon rejected:** A panic or memory safety violation in a native addon crashes the entire VS Code extension host process, which would close every other running extension. This risk is unacceptable for a background computation tool.

**Separate daemon process selected:** The daemon runs in its own process. A crash is contained, detected by the extension host, and handled with automatic restart and backoff as defined in FR-015.

---

## 5. Functional Requirements

### 5.1 Import Detection and Syntax Handling

**FR-001** (Critical) - The extension must detect and correctly process the following ESM import formats in the active document:

| Format                              | Example                                       | Handling                                                                                                                                                                                              |
| ----------------------------------- | --------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Named imports                       | `import { debounce } from 'lodash-es'`        | Extract only the requested named exports and compute their isolated size                                                                                                                              |
| Default imports                     | `import React from 'react'`                   | Evaluate the default export of the module                                                                                                                                                             |
| Namespace imports                   | `import * as _ from 'lodash-es'`              | Evaluate the entire module size since all exports are requested                                                                                                                                       |
| Dynamic imports                     | `import('date-fns')`                          | Evaluate the full module entry size                                                                                                                                                                   |
| Dynamic imports with named bindings | `const { format } = await import('date-fns')` | **v1.0 limitation:** Treated as full module entry size (same as bare `import('date-fns')`). Named bindings on dynamic imports require runtime analysis that is not feasible with static AST analysis. |
| Re-exports                          | `export { format } from 'date-fns'`           | Treat equivalently to a named import of the same specifier                                                                                                                                            |
| Star re-exports                     | `export * from 'lodash-es'`                   | Treat equivalently to a namespace import and mark the syntax as a barrel boundary for UI insight warnings                                                                                             |
| Type-only imports                   | `import type { Foo } from 'bar'`              | Identify and immediately discard; zero runtime cost, must not be sent to daemon                                                                                                                       |
| Type-position-only imports          | `import { Foo } from 'bar'` where `Foo` is used only as a type | Erased by TypeScript under import elision, so zero runtime cost. Elide from sizing in TypeScript documents. A binding referenced anywhere as a value (a class, say) must NOT be elided, nor must an unused import, which `verbatimModuleSyntax` preserves; either would under-count. JavaScript has no type positions and never elides. When every binding of a statement is elided the whole statement is dropped, so the line carries no lens — the same as an explicit `import type`. **Known limitation:** under `emitDecoratorMetadata` TypeScript keeps an import used only as a parameter or property type, because it emits it into `design:paramtypes` metadata. The daemon does not read `tsconfig.json`, so such an import is elided and reported as zero-cost when it has real runtime weight. This affects decorator-heavy codebases (Angular, NestJS, TypeORM) |

The extension must retain the detected import syntax category (`static`, `reexport`, `star_reexport`, or `dynamic`) in its in-memory analysis state so UI features can distinguish normal namespace imports from barrel re-export boundaries without relying on daemon heuristics.

**FR-002** (Critical) - The extension must skip relative imports (those beginning with `./` or `../`).

**FR-003** (Critical) - The extension must skip Node.js built-in module imports, including those prefixed with `node:` and those matching known built-in names such as `fs`, `path`, `os`, `http`, and `crypto`.

**FR-003a** (High) - The extension must skip framework virtual modules and common application aliases that are not npm package dependencies, including `astro:*`, `virtual:*`, `$app/*`, `$env/*`, `$lib/*`, `#imports`, `@/*`, and `~/*`.

**FR-004** (High) - The extension must send supported source documents to the daemon through `AnalyzeDocumentRequest`. Import parsing must be performed in Rust with `oxc_parser`; the extension host must not parse reusable import metadata with `oxc-parser`, the TypeScript Compiler API, regular expressions, or extension-host package resolution.

**FR-004a** (Critical) - **An interactive document response must not wait for an engine build.** `AnalyzeDocumentResponse` must be returned as soon as the daemon has classified the document's imports against the cache: every import the cache can answer carries `status: "ready"` with its result, an import that resolves to no package carries `status: "missing"`, and an import whose engine build has not run carries `status: "loading"` with its resolved `request` and no result. The daemon must then build those imports off the response path and deliver each one to the client as it lands, on the existing `refreshed_results` push (`document_path` + index-aligned `identities` + the analysis generation). The extension must render a `loading` import as a neutral "Calculating..." hint — never as zero bytes and never as an error — and must merge each pushed result into that import's state, dropping any push whose generation a newer analysis has superseded.

  **A pushed result must survive the response it raced.** The daemon writes the response and the first pushes to the same socket, and both frames routinely arrive in ONE read; the client dispatches every frame in a chunk synchronously, so a push reaches the store *before* the awaited continuation that stores the response's states. On a cold document there are no states for it to merge into. On a re-analysis — which is every keystroke, and so the common case — there are: the previous analysis's, which the new response's states then overwrite, push and all. The store must therefore HOLD each push, tagged with the analysis generation it was computed for, and re-apply the pushes of the generation it is storing as part of the same update. A held push from any other generation is dropped, never replayed onto a newer analysis's states — and a push the daemon did not stamp with a generation at all (the SWR refresh of a size read that carried none: the "Show current file size" command's) belongs to no analysis, so it must never be held. It may merge into the states on screen when it arrives, which is all an ungated push was ever owed; replaying it onto whichever analysis lands next is the resurrection that dropping a cleared document's pushes exists to prevent.

  **A replayed push must be captioned by the ANALYSIS, not by itself.** The insights that caption a size — over budget, the git working-tree delta, the shared-module note — are derived from inputs an analysis owns: the `git diff` it ran for that document, the history it read, the budgets in force. A push carries none of them; it can only capture what the extension happened to hold when it landed, which for a document mid-analysis *predates* the diff that analysis is captioned by. The store must therefore recompute the insights of a replayed push with the refiner of the `set` that is replaying it, never with the one the push arrived with — recomputing the whole document from the push's stale inputs takes the working-tree badge off every import in it.

  A streamed import result carries no `shared_bytes`: shared-module weight is a relation between two imports of the SAME document, so it is not knowable until the last of them has been measured. The daemon must therefore close a streamed document with one final `refreshed_results` push carrying the imports whose shared figure the response could not have had right — and must omit that push when nothing in the document is shared, since a zero shared figure and an absent one render identically.

  *What this guarantees:* one package that parks or times out the bundler costs **that import's number**, and nothing else. Every other import in the document — including every one already answered from cache — is delivered on time.

  *What it does not guarantee:* the parked import itself still has no size until its build ends (`BUILD_TIMEOUT`, 8s), after which it is reported **Unmeasured** with a `timeout` stage like any other transient engine failure (FR-026d) — and, being transient, is written to no cache and judged by no budget. The document's own totals (`FileSizeDocumentResponse`) come from a combined build per runtime, which is NOT streamed: a package that parks that build costs the status-bar size for that document, though not its decorations. The workspace report and `importlens check` deliberately keep waiting for every build, because a partial table row and a partial budget check are worse than a late one — a workspace naming enough parked packages can exceed the client's timeout and lose the report.

  *And it does not bound engine pressure — it bounds engine CONCURRENCY.* A `timeout` result is never cached (FR-026c) and there is no durable circuit breaker — a cache-generation-scoped one was tried and it condemned healthy packages, treating a merely-slow build as proof of a deterministic park. So a package that reliably parks the bundler re-parks on **every** analysis of its document, holding one of the two engine permits for the full 8s each time. What is bounded is how many builds can be *in flight at once* for one document, and it is bounded on both paths:

  * *Per-import builds* — a newer analysis flips the previous stream's cancel flag, so its still-queued builds never start, and the single-flight registry (keyed by cache key + cache generation) collapses a concurrent request for the same package into the build already running rather than starting a second.
  * *The combined file-size build* — at most ONE **interactive** build per document at a time. An interactive size read is one tagged with the analysis generation it belongs to (the extension sends one per keystroke's analysis, and they are the only size reads that can stack). Such a request must wait for the document's in-flight combined build, and must then re-check whether a newer size read for the same document superseded it while it waited: if it did, it must answer with a protocol error and run no build at all. Without this, every keystroke in a document stacked another combined build against the two-permit pool, since the connection loop became a multiplexer (FR-004c) and the handlers began running concurrently.

    The bound deliberately does **not** apply to the one-shot size reads — the "Show current file size" command and `importlens check` — which carry no generation. A human cannot stack them, nothing supersedes them, and making them queue behind a parked build (`BUILD_TIMEOUT`, on top of their own) would turn a slow answer into the client's request timeout, which is no answer at all. So the concurrency bound is one interactive combined build per document plus, at most, one user-initiated one — not one per keystroke.

  A build already inside Rolldown cannot be cancelled by anything except its own timeout, which is precisely why that timeout exists. Two reliably-parking packages therefore occupy both permits continuously while the user edits, and every other document's cold imports wait up to 8s for a permit. They still arrive, and no response is ever late — only the numbers are.

**FR-004b** (High) - **`AnalyzeSpecifiers` is deliberately not streamed, and is deliberately not time-bounded.** Its only callers are one-shot commands (Compare Imports, named-export candidates). They hold no per-import decoration rows for a push to merge into, and a comparison assembled from half-measured imports is worse than an honest "comparison failed" — so this one request still waits for every engine miss it names. Its total time is bounded only by `BUILD_TIMEOUT` **per build**, times the number of misses it must drain two at a time; there is no request-level budget (the previous `EngineBudget` was deleted with the atomic response it existed to protect, and it bought its bound by fabricating the sizes it cut off). The extension's own request timeout is what fails the command. The connection loop is unaffected: like every other request, it runs as a task, so a slow comparison cannot delay another document's analysis or its streamed imports.

**FR-004c** (Critical) - **The daemon's connection loop must be a pure multiplexer.** It does exactly three things: read frames, dispatch each request to a task, and write outbound messages. No handler may be awaited inside the loop body, and **every** response must leave through the same outbound channel as the unsolicited pushes — so that nothing an individual handler does can stall the delivery of a frame belonging to anything else. Responses keep their per-request correspondence (`request_id`), but there is no ordering guarantee *between* requests: a fast one may overtake a slow one, which is the point.

  This requirement exists because FR-004a is otherwise undeliverable. A loop that awaits its handlers inline sits suspended *inside the arm* rather than in its `select!`, so while a request is being served it can neither write a queued push nor read a new frame. The extension sends `AnalyzeDocument` and then immediately `FileSizeDocument` for the same document; with an inline await the file-size combined build held every streamed import behind it for the whole build timeout, and held the *next* analysis's frame unread past the client's 10s deadline — at which point the extension discards the entire document, every cache hit included. The import results were computed on time and simply never written to the socket.

  The outbound channel is unbounded, and bounded in practice: the loop is the socket's only writer, so a client that stops reading backs the write up in the loop, which then stops reading frames and admits no new work. What can still accumulate is the work the client already asked for — one response per in-flight request, one push per still-building import — never anything the daemon generates on its own.

  **Ending the connection: cancel, then join under a bound, then flush — in that order, on EVERY path.** Every task the loop spawns to serve a request or produce a push must be registered, so that shutdown, an idle recycle and a lost connection can account for it. Ending the connection must then:

  1. **Cancel everything cancellable**, *before* waiting for anything: the prefetch jobs (NFR-004c), the still-draining registry-refresh blocks, the streamed-import builds a document analysis handed off, the SWR revalidations, the queued combined builds, and the scheduled cache-maintenance pass. Cancellation is a flag each job re-reads before it starts its next unit of work, so what has not started never starts.
  2. **Join the registered tasks under a deadline** (`TASK_JOIN_TIMEOUT`, 2s), not indefinitely.
  3. **Flush the cache unconditionally**, whether or not every task finished.

  **Every path** means every path: the client's `shutdown`, an idle recycle, **and a connection that simply ends** — the client closed the pipe, or the extension host crashed without ever sending `shutdown` and the daemon read EOF, or the socket failed mid-write. There must be exactly one teardown, because when there were three only one of them flushed: an extension-host crash left every import that session had measured in memory, to be rebuilt from scratch next session. The cache does not care *why* the connection ended. Nor may the flush be left to `Drop`: `Drop` reaches only the entries already queued for the batched disk commit — never one whose insert failed and is held dirty for replay, nor the recency a session's cache hits earned — and it is not guaranteed to run before the process exits at all.

  Only a client that ends the connection **on its own terms** is owed the frames its tasks queued on the way out, so `shutdown` and the idle recycle drain the outbound queue after the flush; a client that has vanished is owed nothing, and the EOF and socket-error paths do not drain.

  The bound is the requirement, not an implementation detail, and this is what it buys. One class of work cannot be cancelled at all: a build already inside Rolldown runs to `BUILD_TIMEOUT` (8s) and nothing in the daemon can stop it. The extension force-kills the daemon 5s after sending `shutdown`. An unbounded join therefore *guarantees* the kill lands first and `flush_cache` never runs — the graceful shutdown loses the entire session's unwritten cache to save one build's result, which, if that build hit its timeout, was never cacheable in the first place (FR-026c). So the daemon must be explicit about what it does NOT guarantee: **a task still running at the deadline is abandoned.** Its result is not persisted, and it is recomputed next session. What IS guaranteed is that every finished task's work is flushed, every frame it queued is delivered, and nothing that could be stopped was left running.

  An idle recycle additionally requires that nothing be in flight at all: it must reap the handles of tasks that have already finished (a finished handle is not work) and, if any task is genuinely still running, defer to the next check rather than cancel it — an idle-timer recycle has no user waiting on it and no right to discard work the user asked for.

**FR-005** (High) - The daemon must use OXC parser module-record output to extract imports directly from `staticImports`, `staticExports`, and `dynamicImports`. When OXC returns recoverable module information while the user is mid-typing, the daemon must extract as much structural information as possible. If the parser cannot produce usable module information, the daemon must return an empty or unavailable analysis without showing a blocking editor error.

**FR-006** (Critical) - The extension must debounce document-analysis requests by the value configured in `importLens.debounceMs` (default 300ms) after the last document change event. Requests must not be sent on every keystroke.

**FR-006a** (Critical) - The daemon must support Svelte documents by extracting imports from every `<script>` block, including module-context and instance scripts. `<script lang="ts">` blocks must be parsed as TypeScript and all detected import positions must map back to the original `.svelte` document.

**FR-006b** (High) - The extension must support Astro documents by extracting imports from frontmatter and processed client `<script>` blocks. Frontmatter imports must be marked as `server` runtime; processed client script imports must be marked as `client` runtime. Inline Astro scripts with non-processed attributes such as `is:inline` must not be treated as bundled imports.

**FR-006c** (High) - The extension must support local JS/TS files opened outside a VS Code workspace folder. For such loose files, the extension must derive an analysis root by walking upward from the file to the nearest `package.json` or `node_modules` directory and must start the daemon with that derived root. If neither exists, the file's containing directory is used as the fallback root. Loose-file support must use the active document path for package resolution and must not display daemon unavailable solely because no workspace folder exists.

**FR-006d** (High) - The daemon must support Vue Single File Components by extracting imports from every `<script>` block, including `<script setup>` and classic scripts. `<script lang="ts">`, `<script lang="tsx">`, and `<script lang="jsx">` blocks must be parsed with the matching language mode, and all detected import positions must map back to the original `.vue` document.

### 5.2 Package Version Resolution

**FR-007** (Critical) - The daemon must resolve each package by searching upward from the active document path, reading `node_modules/<package>/package.json`, and extracting the `version` field when it is present as a string. For scoped packages (e.g. `@babel/core`), the path is `node_modules/@<scope>/<name>/package.json`. The `<package>` identifier in all cache keys and IPC messages includes the full scope prefix when present. If the package directory exists but the manifest is malformed or lacks a string `version`, the daemon must use an unknown-version sentinel and report the import **Unmeasured** with a `package_manifest` stage (FR-026d).

**FR-007a** (High) - The daemon package resolver must search upward from the active document path, not from the first workspace folder. This mirrors Node resolution in nested workspaces and loose-file windows.

**FR-008** (High) - The daemon resolver must start package discovery and module resolution from the `active_document_path` supplied in `BatchRequest`, not from the workspace root. Starting from the file being edited ensures that upward traversal through the directory tree matches Node's own resolution algorithm exactly. This is critical in multi-root VS Code windows, NPM Workspaces, Yarn Workspaces, and nested PNPM layouts where a package inside `packages/app-a/` may have its own `node_modules/` with a different version of a dependency than the root-level hoisted copy. The daemon must validate package identifiers before building filesystem paths and must reject identifiers containing traversal or platform path separators.

**FR-009** (High) - If a package cannot be found in `node_modules`, the extension must display a subtle "Package not found" decoration on that import line and must not send it to the daemon. This missing-package path applies only when the package directory cannot be located; an installed package with a malformed or versionless manifest is still sent to the daemon, which reports it Unmeasured (FR-007, FR-026d).

### 5.3 Daemon Communication

**FR-010** (Critical) - The IPC protocol must use MessagePack as the serialization format on both the TypeScript and Rust sides.

**FR-011** (Critical) - Messages must be length-prefixed with a 4-byte big-endian unsigned integer representing the byte length of the MessagePack payload that follows. This allows the socket to handle concurrent in-flight requests without message boundary ambiguity. Both the TypeScript and Rust decoders must reject frames larger than 32 MiB and must validate frame length arithmetic before allocating a payload buffer. The Rust IPC server must use `tokio-util` length-delimited framing configured for the existing 4-byte big-endian prefix and 32 MiB maximum frame size; the TypeScript decoder keeps the compatible custom frame decoder.

**FR-012** (Critical) - The extension must send all imports from a single debounce cycle as a single `BatchRequest`, not one request per import line.

**FR-013** (High) - The extension must implement request cancellation using the `request_id` field present in both `BatchRequest` and `BatchResponse`. Each debounce cycle must increment a monotonic counter and send it as `request_id`. If a response arrives whose `request_id` does not match the most recently sent request, the extension must discard it without updating decorations. This makes cancellation unambiguous regardless of response timing; timing-based heuristics must not be used.

**FR-013a** (High) - When the daemon encounters a computation error for one or more imports in a batch, it must return a partial `BatchResponse` containing successful results for all other imports in the same batch. For each failed import, the `ImportResult.error` field must be set to a non-null string describing the failure reason, `ImportResult.diagnostics` must include at least one structured diagnostic entry with the failing stage and real daemon context, and all numeric size fields must be set to `0`. The extension host must render a subtle "Size unavailable" decoration for imports whose `ImportResult.error` is non-null, and must not show a user-visible error dialog. The extension host must keep raw diagnostic details out of the inline UI while making them copyable from the hover.

**FR-013b** (High) - Protocol v2+ clients may request streaming batch responses by setting `BatchRequest.streaming: true`. In streaming mode, the daemon must emit partial `BatchResponse` frames as import results become available and set `BatchResponse.indexes` to the zero-based import indexes represented by that frame. The IPC server must write each partial frame to the socket while the rest of the batch is still computing; it must not buffer all partials in memory and flush them only after the final result is ready. This index list is required because duplicate specifiers can appear multiple times in one file. A final full-batch `BatchResponse` with shared-byte annotations must still be emitted for compatibility with existing request-state handling. Protocol v1 clients and v2+ clients without `streaming: true` receive only a full batch response.

**FR-013c** (High) - Protocol v5 and newer clients may request streaming `package.json` dependency analysis by setting `AnalyzePackageJsonRequest.streaming: true`. In streaming mode, the daemon must first emit a names-only partial `AnalyzePackageJsonResponse` with `indexes` covering every dependency entry, `status: "loading"`, and no `installedVersion`, so clients can render dependency rows before filesystem package resolution finishes. The daemon must then emit a resolved loading partial covering those same indexes: dependencies whose installed package version was resolved remain `status: "loading"` with `installedVersion`, while dependencies that cannot be resolved may be emitted as `status: "missing"`. As each package size result becomes available, the daemon must emit an indexed partial response for that dependency. The final `AnalyzePackageJsonResponse` must omit `indexes` and contain complete size states, including shared-byte annotations where applicable. The extension host must merge indexed partials without overwriting newer daemon-provided registry hints.

**FR-014** (High) - On socket disconnect, the extension must discard any stale MessagePack payloads currently in the receive buffer and wait for the next document change event to trigger a fresh request cycle.

**FR-015** (High) - If the daemon process crashes, the extension must detect the disconnection, wait 1 second, and attempt to restart the daemon. On subsequent failures, it must apply exponential backoff (1s, 2s, 4s, 8s, capped at 30s). After three consecutive failures within 60 seconds, it must enter degraded mode and display a status bar notification.

**FR-015a** (High) - The extension host must pipe daemon process output into the Import Lens output channel. Structured daemon log lines use the format `[import-lens-daemon] <ISO8601> [<LEVEL>] [<component>] <message>`; the host parses level and component and applies `importLens.logLevel` before display. Unparsed stdout lines map to info and unparsed stderr lines map to warn for backward compatibility. The default log level is `info` so the status-bar "Show Logs" path contains useful startup diagnostics without extra configuration. Failed startup after process spawn, including IPC connect failure or hello-send failure, must dispose any created IPC client and terminate the child daemon process before scheduling restart or entering degraded mode.

### 5.4 Size Computation

**FR-016** (Critical) - For each cache-miss import, the daemon must construct a virtual ESM entry module in memory whose synthetic targets map to the pre-resolved package entry paths, using the alias forms specified in Section 10.3:
- Named imports: one uniquely aliased string-literal re-export per requested name
- Default imports: a uniquely aliased default re-export
- Namespace, dynamic, and full-package requests: the escaping-namespace form (`import * as` then `export`), because `export * from` would drop the target's default export

Every requested surface must carry a unique entry alias so strict entry signatures keep it alive; the virtual entry must never use `console.log` or any pattern that can be statically eliminated by a tree-shaker, and user-controlled names must be serialized as escaped string literals, never interpolated raw.

**FR-017** (Critical) - The daemon must use `oxc_resolver` to resolve the package entry point from `node_modules`. The resolver must use the following `exports` condition set, in priority order: `["module", "import", "default"]`. This selects the ESM path when available, which is required for accurate tree-shaking. The `"require"` condition must not be in the set; its presence would cause `oxc_resolver` to prefer CJS paths on packages that publish both. If no ESM entry can be resolved, the daemon falls back to the `"main"` field and sets `is_cjs: true` in the response. The resolver must also respect the `"browser"` field for packages that use it as an ESM entry alias. The `"module"` top-level field (used by older packages before the `exports` map existed) is respected as a lower-priority fallback after `exports` map resolution. This direct root resolution happens before any bundler build so cache identity and fast cache hits never construct a bundler. Transitive imports are resolved exclusively by the Rolldown engine, whose resolve options (condition names, main fields) mirror the direct resolver's configuration per runtime so the two cannot disagree on entry semantics. Node builtins, unresolved peers, and other externals must remain external boundaries in the linked output and must produce structured diagnostics rather than failing the whole import when partial analysis can continue.

**FR-017a** (High) - If package entry resolution fails but the installed package directory contains declaration files (`.d.ts`, `.d.mts`, or `.d.cts`) and no runtime JavaScript or TypeScript source files (`.js`, `.mjs`, `.cjs`, `.jsx`, `.ts`, `.tsx`, `.mts`, or `.cts`, excluding declaration files), the daemon must return a successful zero-byte `ImportResult` instead of marking the import unavailable. The result must set all byte fields to `0`, `side_effects: false`, `is_cjs: false`, and include a structured `types_only` diagnostic so the extension can label the import as declaration-only runtime cost.

**FR-018** (Critical) - The daemon must perform module linking and tree-shaking through the embedded Rolldown bundler behind the engine contract in Section 10.7. The pipeline is:
1. Construct a virtual ESM entry module (as defined in FR-016 and Section 10.3) whose synthetic targets map to the pre-resolved package entry paths from FR-017.
2. Run one Rolldown build over the virtual entry. Rolldown exclusively owns transitive resolution, module loading, ESM/CJS linking, binding and namespace semantics, symbol deconfliction, TS/TSX/JSX/JSON handling, and statement/module retention (tree-shaking). The daemon must not re-implement, override, or post-correct any of those semantics.
3. The engine's native plugin enforces hard limits of 2,000 internal modules, 20 MiB per module source file, and 100 MiB total module source bytes. A breached limit is a typed `module_graph_limit` failure, never a partial graph.
4. The build must emit exactly one unminified ESM **chunk**; more than one means the graph was code-split, which cannot be measured from a single chunk without under-reporting the rest, and is a typed `output_shape` failure. Cycles link without duplicate module inclusion; dynamic imports inline into the single chunk (code splitting is disabled). A package's non-JavaScript files never reach the output as **assets** at all — Rolldown 1.1.5 fails the whole build at the link stage rather than emitting one — so the plugin records their bytes at `load` and links them as empty modules (FR-018a, Section 10.7).
5. The daemon validates the linked chunk with OXC semantic analysis and minifies it per FR-019. An engine or validation failure produces an **Unmeasured** result under the stage it failed at (FR-026d) — it must never fabricate a binding, measure partially linked code, or substitute a size.

**FR-018a** (High) - **A package's non-JavaScript bytes are disclosed, never counted and never fatal.** A stylesheet that the package's own JavaScript entry imports is real shipped weight that this tool does not measure — the measured size is the JavaScript chunk. The daemon must:

  * link the stylesheet as an empty module so the JavaScript graph can be measured at all. Rolldown 1.1.5 removed CSS bundling, and a `.css` module reaching it fails the **entire build** at the link stage (`UNSUPPORTED_FEATURE`). Every package whose ESM entry does `import './styles.css'` — most UI kits — was therefore unmeasurable, which nobody could see while a failed build was still given a fabricated size;
  * emit an `uncounted_assets` diagnostic naming those files and totalling their bytes;
  * hold the result at **Medium** confidence. This is a decision, not an oversight: a number that omits bytes the user's bundle will really carry is not a High-confidence measurement of that package's cost, and claiming otherwise is the same overclaim — one order of magnitude smaller — that FR-026d exists to stop. It is also what the neighbouring `external` diagnostic already does for the same reason. Medium carries no `~` prefix (FR-031 reserves that for Low), so such a package reads as a plain number with a stated caveat, which is exactly what it is.

**FR-019** (Critical) - The daemon must use `oxc_minifier` to perform dead code elimination, constant folding, and supported identifier mangling on the tree-shaken output, then use `oxc_codegen` (with `minify: true`) to emit the minified JavaScript string. Codegen must use the scoping and private-member mappings returned by `oxc_minifier::Minifier::minify`; the daemon must not run a second independent mangling pass over already-minified AST state.

**FR-020** (Critical) - After minification, the daemon must compute three compressed sizes in parallel: gzip using `flate2` at level 6, Brotli using the `brotli` crate at level 4, and zstd using the `zstd` crate at level 3.

**FR-021** (Critical) - Rolldown is the only semantic authority for `package.json#sideEffects`: it natively interprets boolean, string, and array forms plus nearest-transitive-package metadata, and the daemon must not override its retention decisions with a hook or an AST purity check. The daemon reads the root package's `sideEffects` field separately as **reporting metadata only** — it decides a badge, never a byte — and **`side_effects` is a property of THE IMPORT, not of the package**: the question it answers is whether *the entry being measured* is one the package declares effectful. It sets the response fields:
- If the field is `true` or absent: the response sets `side_effects: true` and `truly_treeshakeable: false`.
- If the field is `false`: the response sets `side_effects: false`.
- If the field is a glob — an array of patterns, or the single-pattern string form (e.g., `["*.css", "dist/polyfill.js"]`) — the response sets `side_effects` to whether **the resolved entry matches one of them**. A package declaring `["**/*.css"]` is therefore **not** side-effectful for a JavaScript import: the rule says nothing about that entry, and such a package may be `truly_treeshakeable` at High confidence like any other. The match is performed with **`fast_glob` — the same crate Rolldown matches `sideEffects` with** (`rolldown_common`, `rolldown_utils`), normalized the same way; the crate is exact-pinned into the coordinated compiler stack for exactly that reason. Rolldown still applies the declared globs to actual retention, and this reading never reaches it.
  - **Sharing the matcher is necessary and NOT sufficient, and the daemon must satisfy the other half too: both sides must feed that matcher the SAME PATH.** A `sideEffects` glob is matched against the entry's **package-relative** path, and the two sides derive that path independently — the daemon by stripping the package root from the entry, Rolldown by relativizing the module id it was given against the directory of the `package.json` path it was given (`resolved_id.id.relative_path(package_json.realpath().parent())`). **Rolldown does not re-derive either input**: `try_get_package_json_or_create` takes the manifest path from the plugin verbatim. So both inputs are Import Lens's to get right, and if they do not share a root the relativization silently degrades to the whole absolute path. It did: the entry id was `fs::canonicalize` output — a Windows `\\?\` verbatim path — while the manifest path was built from the non-canonical document path, so `refractor` (`"sideEffects": ["lib/all.js","lib/common.js"]`, entry `lib/common.js`) was matched as `\\?\C:\…\node_modules\refractor\lib\common.js`, matched nothing, was tree-shaken as pure, and was reported at **30,229 B** against a true **113,152 B** — a 3.7x undercount. It hid because the *matcher's own normalisation* prefixes `**/` to any pattern with no separator or a `./` prefix, and `**/` matches an absolute path perfectly well; only a pattern **containing a `/`** is anchored, and no test used one. Therefore: the daemon must hand Rolldown a **canonical** manifest path beside its **canonical** entry id — canonical on both sides is the only spelling that also survives a pnpm store link and a workspace-linked package's junction — and must derive its own badge path by **stripping the package root** it already holds, never by scanning the entry for a `node_modules` component (a workspace-linked package's real path has none, which sent every monorepo-internal package to `Unknown` ⇒ side-effectful ⇒ `truly_treeshakeable: false` by construction, while Rolldown dropped its effects as pure). Only with the same matcher **and** the same path can the badge and the retention not disagree about one declaration, and `daemon/tests/analyze.rs` pins every declaration form — slash-bearing patterns and a workspace-linked package included — against the bytes Rolldown actually retained.
  - **The pattern list is read exactly as `oxc_resolver` reads it** — the parser whose output Rolldown builds its own `SideEffects` from — so the two cannot disagree about *what the list is* either, having been made unable to disagree about what it matches. Two forms are degenerate, and both mean **`side_effects: false`** because an `.any()` over no usable pattern matches nothing, which is precisely how Rolldown answers them: an **empty array** (`"sideEffects": []` means what `false` means, and Rolldown retains the same bytes for both), and an array carrying a **non-string element** (`oxc_resolver` collects the array with `filter_map(as_str)`: the element is dropped, not fatal — so `["index.js", 42]` still matches `index.js`, and `[42]` is the empty list again). The daemon must not refuse to read a list Rolldown reads without complaint: a refusal reports `side_effects: true` over a size Rolldown produced by tree-shaking that entry as **pure**, which forces `truly_treeshakeable: false` by construction and caps confidence — a badge contradicting the build its own number came out of. A value that is not a bool, a string or an array is a different case: `oxc_resolver` surfaces no declaration at all, Rolldown falls back to analysing the source, and the daemon — which FR-024 forbids that analysis — stays conservative (`side_effects: true`), as it does for an absent field.
  - Superseded on 2026-07-14: the array form used to report `side_effects: true` *whatever it matched*, with `truly_treeshakeable: false` and a conservative `side_effects` diagnostic, on the premise that matched paths were "not available from public bundler metadata". That premise was retracted by the Section 10.7 amendment (Rolldown matches string and array globs correctly, on Windows included), and the conservatism it bought made **every** package declaring an array — an everyday declaration — permanently side-effectful, never tree-shakeable, and never High confidence. The empty-array and non-string-element forms outlived that fix by one commit: they short-circuited to "unknown" *before the matcher was consulted*, so `"sideEffects": []` — which Rolldown treats as fully pure, measured — was still reported side-effectful, never tree-shakeable and never High confidence.

**FR-022** (High) - The daemon must detect when a package is not genuinely tree-shakeable by comparing the named-export minified size against the full-package minified size. If the named-export minified size exceeds 95% of the full-package minified size, `truly_treeshakeable` must be set to `false` in the response.

**FR-023** (High) - The daemon must process all imports in a single `BatchRequest` concurrently. Resolve-only work and cache classification remain on the global Rayon thread pool, which must be sized to `max(1, available_parallelism - 2)` to leave headroom for VS Code's renderer and extension host threads (`std::thread::available_parallelism()`; the `num_cpus` crate must not be used). Cache misses that require a bundler build must run as async work behind a daemon-wide two-permit execution boundary and must never be invoked from an outer global-Rayon parallel loop, because Rolldown owns its own internal Rayon parallelism and nesting the two oversubscribes the pool. Cache hits bypass the boundary and never construct a bundler. Batch and file-size responses must preserve input ordering even when misses complete out of order; streaming responses may emit in completion order with their existing indexes.

**FR-024** (Critical) - The Rust daemon must operate exclusively via static AST analysis. It is prohibited from evaluating, executing, or interpreting any code found within third-party packages. No `eval`, subprocess execution, or dynamic code loading of any kind is permitted.

**FR-024a** (High) - CommonJS support is provided by Rolldown's link-time ESM/CJS interop, which is static analysis (the FR-024 prohibition on evaluation holds; Rolldown never executes package code). Named access into a CJS module works through interop binding at link time; a CJS package without granular module boundaries retains its whole library, which is the correct measured cost. Export enumeration for CJS entries reads the linked chunk's export list and therefore may surface only `default` even when `exports.name =` assignments are statically visible — the daemon must not guess additional names (Section 10.7's never-guess rule). An engine failure on a CJS entry is Unmeasured like any other (FR-026d); it is **not** sized from the entry file alone. File-level size requests must still return conservative totals with diagnostics rather than reporting zero bytes.

  A conservative total is a **floor**, not the file's size, and the daemon must be able to tell the difference. When a runtime group's combined build fails, its totals degrade to a sum of the per-import measurements the caller already holds — and only a **Measured** import has one to give. Every other contributor adds exactly zero, so the sum is short by an unknown amount:

  * an import whose own build has not landed yet (`status: "loading"`, FR-004a);
  * an import a **transient** engine failure left Unmeasured;
  * an import a **deterministic** failure left Unmeasured;
  * an import whose entry could not be resolved at all — not even an entry of the combined build, so its bytes are absent from the totals however cleanly that build goes. This includes an import whose package **is not installed**: it has no version, so the daemon cannot even build a request for it, and it must still reach the aggregate rather than being filtered out of its input before it can be counted as missing.

  **Any one of them makes the total a floor**, which must be flagged `incomplete`, returned anyway (a floor beats a zero) with a diagnostic naming the specifier and its stage, and **never** written to the L1 aggregate cache (FR-026c), persisted (FR-026c), or judged (FR-032a).

  **A declarations-only import is NOT one of them.** It resolves to no entry *because it ships no runtime code*, and it is answered **Measured** at zero (FR-026d, `types_only`). Its zero is a fact, so the total stays complete. Flagging it `incomplete` — which a naive "any unresolved entry is a gap" check does — makes every file importing an `@types/…` package a permanent floor: the combined build re-runs on every size request, nothing is ever cached or persisted, and `importlens check` exits 3. That is a large fraction of real TypeScript. "Resolved to nothing" and "could not be resolved" are different answers.

  **And a PATH ALIAS is not one of them either.** "No `node_modules/<name>/package.json`" is two different facts, and the daemon must tell them apart (`pipeline::resolver::resolves_to_first_party_source`):

  * the specifier **resolves, through the project's tsconfig `paths` / `baseUrl`, to a real file outside `node_modules`** — a path alias (`@app/components`, `~lib/foo`, a bare `components/Button`), pointing at first-party source. Import Lens measures third-party imports ([ADR-0004]), so first-party code contributes nothing to any total it reports, exactly like a relative import — which is never even detected. Its zero is a fact, not a gap: it is reported with a `path_alias` diagnostic and flags **nothing**. (`@/…`, `~/…`, `#…` and `$…` never reach this point at all — FR-003a drops them before detection, which is why the regression only ever bit the alias forms that *look* like package names.)
  * the specifier **resolves to nothing** — an uninstalled dependency, a typo, an import left behind by a `pnpm remove`. Floor, as above.

  **The target does not have to sit inside the workspace root.** A file that **exists** and is **not** inside `node_modules` is first-party source wherever it sits — the project's own tsconfig says so — and it ships no npm-package bytes, so it must flag nothing. The `node_modules` test is what keeps a real package from being mistaken for source, and it is the only bound the target needs. A previous revision also required the target to lie inside the workspace root, mirroring the bound on the *config*; the two are not the same rule, and that one made **opening one package of a monorepo** (an ordinary way to open one) turn every file using a cross-package alias — `"@shared/*": ["../shared/*"]` — into a **permanent floor**: never cached, and refused a verdict by `importlens check`. A sibling package's source is the user's own code.

  **A floor must never be sticky.** The daemon memoizes the `references` walk and **no filesystem fact**: the alias resolvers are rebuilt per query. An import written *before* the file it points at is correctly a floor, and creating that file must lift it on the next request — with no restart and no invalidation message, because nothing watches first-party source and so no message could ever arrive. A resolver that outlives the query negative-caches the missing path in `oxc_resolver`'s filesystem cache, and the daemon's first answer becomes its answer forever: a cached negative that nothing can invalidate, which is the same defect as the config the daemon read exactly once (FR-027a), one level down.

  **The discriminator is positive evidence of first-party source, and it is never the absence of a `package.json` declaration.** Bullet 4 says "not installed" and means it: `import _ from 'lodash'` omits exactly the same bytes from this file's total whether or not `package.json` names lodash, so declaration cannot decide the question. An earlier attempt made it the discriminator and had to narrow this bullet to fit — an import that was neither declared nor installed was then read as an alias and flagged nothing, so a total short a whole package was cached, persisted as the file's baseline, and passed by `importlens check`. That is the silent pass ADR-0006 exists to abolish, and no convenience buys it.

  **The alias table is a property of the WORKSPACE, never of the importing document — and never of one project inside it.** The question is *"does this specifier map, through **any** `paths` table this workspace reaches, to a first-party file that exists?"*, and the daemon must answer it the same way whichever document asks and whatever order the project's configs happen to be listed in. So it locates the nearest `tsconfig.json` or `jsconfig.json` at or above the document — **bounded at the workspace root**, so a config outside the project can never decide whether one of its imports is first-party — then collects **every** `paths` table reachable from it (that config, every project in its `references`, transitively, and the `extends` chain each of those folds in) and tries the specifier against **each**. One hit is the positive evidence; no hit is a floor.

  **One unloadable `references` entry may cost only its own table.** The daemon enumerates a config's references entry by entry and skips what will not load. A `references` list naming a `tsconfig.node.json` somebody deleted is ordinary, and asking `oxc_resolver` to resolve a config *together with* its references fails the whole load if any single one of them cannot be read — so one stale entry enumerated **nothing**, the sibling that owns the only `paths` table was never asked, and every alias in the workspace became a floor.

  Each config is handed to `oxc_resolver` explicitly (`TsconfigDiscovery::Manual`, `TsconfigReferences::Disabled`), and both halves are load-bearing:

  * `Manual` is required because automatic discovery applies a config only where its `files` / `include` / `exclude` **claims the importing document**, and TypeScript's default `include` claims no `.vue`, `.svelte` or `.astro` file — three of the six languages the extension activates on. Keyed that way, the same alias in the same project resolved from a `.ts` document and resolved to *nothing* from the other three, so **every Vue, Svelte and Astro file that used a path alias was a permanent floor**.
  * `Disabled` is required because under `Auto` a resolver loads a config's `references` when it loads the config, and **fails the whole load if any one of them cannot be read**. A config that owns a good `paths` table and lists one deleted project would then resolve nothing at all, and every alias in *that* table would become a floor. `Disabled` drops the references before they are loaded, so the config's own table still answers; the graph is walked by the daemon instead (above), which is where a bad entry is skipped.

    *This requirement was previously justified differently, and that justification was measured false and is withdrawn.* It said `Auto` would make the resolver pick one referenced project by **`references` list order** (create-vue lists the `paths`-less `tsconfig.node.json` first), killing the solution-style scaffold. That was true of the design in which oxc chose the project. It is not true of this one: the daemon walks the `references` graph itself and asks **every** table, so no pick is made and flipping the option to `Auto` leaves the whole suite — the matrix included — green. The requirement stands on the reason above, which has a test that goes red without it.

  The two errors are not symmetric, and the asymmetry is deliberate. An alias the daemon cannot see resolve is treated as a missing package and its file becomes a **floor**: conservative, never a wrong number. Reading an alias as a missing dependency *by default*, by contrast, makes **every file that uses path aliases a permanent floor** — never cached, never persisted, and refused a verdict by `importlens check` — and path aliases are ordinary in real TypeScript projects.

  **The claim must be measured, not asserted.** This paragraph previously stated that solution-style configs "still work". They did not, and no test covered a `references` config at all. The requirement is therefore a **matrix**: config shape × importing document — flat `tsconfig.json` (default `include`), flat with `"include": ["**/*"]`, solution-style with the `paths` project listed first, solution-style with it listed **last** (the order create-vue ships), solution-style with a **dangling reference**, `jsconfig.json`, `extends` a base config that holds the `paths`, and a **monorepo opened at one package** whose alias target sits *above* the workspace root — each asked from a `.ts`, a `.vue`, a `.svelte` and an `.astro` document, and every one of the thirty-two cells must resolve. Beside it, from every document type: a package that is **not installed** (declared or not), a **typo**, an alias whose **target file does not exist**, and a bare specifier with **no config at all** — every cell a **floor**.

  **The residual limits, stated rather than claimed away.** These resolve to nothing and therefore land on **floor** — conservative, never a wrong number:

  * an alias declared *only* in a Vite, webpack or Rollup config. The daemon reads `tsconfig.json` / `jsconfig.json` and the `extends` / `references` chains from them, and nothing else. The repair is to mirror the alias into tsconfig `paths`, which TypeScript itself already requires in order to type-check the file — **and that repair must actually take effect**, which is FR-027a;
  * an alias whose target file does not exist (the *pattern* matching is not evidence; the file is);
  * a `references` graph wider than **24 reachable configs**, whose tail is not walked. The cap stops one config that references a hundred projects from turning one unresolvable specifier into a hundred resolver builds; real scaffolds have two or three.

  And one that does **not** land on floor, and must be stated because it is the price of a document-independent answer: **every reachable `paths` table is asked, so an alias defined only in `tsconfig.node.json` also resolves for a document governed by `tsconfig.app.json`.** Nothing tells the daemon which project owns a document once `include` is discarded (which `Manual` requires, above), and this errs toward *flag nothing* for a specifier that really is first-party source somewhere in the workspace. It cannot invent a number: the specifier still resolves to a file that exists outside `node_modules`, which weighs nothing in either project.

  The memo of reachable-config graphs is capped at **64 nearest-config paths**; overflow clears it, costing a re-walk and nothing else. It holds config paths, never measurements.

  **And the aggregate's OWN build can fail with every contributor Measured.** A File Cost is one bundle over all the file's imports, so a module two of them share is counted **once**. When the combined build fails, what is left is a **sum of per-import costs** — which [ADR-0004] calls a *Combined Import Cost*, a **different quantity**, in which that shared module is counted **twice**. It is an *over*-count, not a floor, so `incomplete` is not the flag for it and cannot be: every contributor really was measured, and `error` is `None` because the fallback sum succeeded. It must carry its own flag, **`degraded`**, on the wire (`FileSizeResponse`, `FileSizeDocumentResponse`) — and it is the likeliest failure in the system, because a combined build is strictly larger than any single import's build and so the first thing to hit `BUILD_TIMEOUT`.

  `degraded` is set for a **deterministic** combined-build failure too. An over-count cannot produce a false *pass*, but it can produce a false **FAIL**, and a budget judged against a number the file never had is neither passed nor failed (FR-032a).

  Shown, never stored, never judged — the same three rules as a floor, for the opposite error.

  The deterministic case was once exempted here, on the reasoning that its bytes are unknown *forever*, so the total is "as complete as this file can ever be". That is wrong twice over, and it was the seventh instance of the defect FR-026d exists to end. Deterministically-unknown bytes are still unknown, and a total is a question about bytes. And the same deterministic failure **also kills the file's combined build**, so the number on offer is an un-deduplicated per-import sum — which changes the contribution of every import that *was* measured. With `incomplete` clear, that number passed every downstream gate at once, because all of them read the one flag: it was cached for the L1 TTL, persisted to the no-TTL bundle-impact history as the file's permanent baseline, shown with no estimate label, and passed by `importlens check` with **exit 0**.

  A per-import deterministic failure is still **cached as a per-import fact** (FR-026c) — it is a property of the package's bytes. The two rules are about different things, and conflating them is what produced the defect: caching the *failure* is right, caching the *file total that is missing it* is not.

**Known limitation (accepted 2026-07-12): no named-CJS typo warning.** The pre-Rolldown analyzer emitted *"named CommonJS export(s) not found"* from its own scan of the CJS entry. That warning is gone and is not being restored. Interop exposes a CJS entry's surface as `default` only, so there is no validated name set to check an import against, and re-adding one would mean re-introducing exactly the hand-rolled, regex-grade module analysis the Rolldown cutover exists to delete — for a lint, at the cost of a second source of truth for CJS semantics.

This is one trade, not two independent losses. The same synthetic namespace that costs the typo warning is what makes CJS packages **immune to the type-only-import elision hazard**: because named access resolves through interop rather than a checked export list, Rolldown never raises `missing_export` on a CJS named import. A mistyped named import of a CJS package is therefore reported at the package's real weight rather than flagged — a missing lint, never a wrong number.

**Implementation status note:** The daemon runs the Rolldown engine for all size-producing paths (individual analysis, full-package comparison, export enumeration, prewarm, and combined file sizing). When the engine cannot safely produce a trustworthy bundle, the import is reported **Unmeasured** with structured diagnostics (FR-026d) — it is not given a static estimate, and the successful imports of the same request keep their real numbers.

### 5.5 Caching

**FR-025** (Critical) - The daemon must maintain an in-memory cache using a `papaya::HashMap`. Cache keys must use the structured v4 identity format described in Section 10.2, including analyzer version, package identity, runtime profile, import kind, sorted named exports, and resolved package paths when known. File fingerprints are NOT part of the key (identity is pure); they are stored on the value side and verified through the tri-state freshness check on every serve. Valid, fresh cache hits must be returned without running any computation.

**FR-026** (Critical) - When `importLens.enableDiskCache` is `true` (the default), the daemon must persist computed cache entries to `redb` databases under an extension-owned cache base. VS Code Desktop must prefer the workspace-specific `ExtensionContext.storageUri` cache base and fall back to `globalStorageUri/workspace-cache` only when workspace storage is unavailable. The daemon must create one stable project shard per normalized analysis root under that cache base, so multi-root windows and loose-file projects do not share one growing database. The extension and daemon must not create cache folders inside the user's project tree. On startup or first project use, the daemon must preload only the configured bounded recent-entry set into the matching project's `papaya` cache; other valid disk entries remain available through lazy disk lookup and are promoted into memory on first hit. During upgrade from the previous centralized-cache design, the daemon must remove the legacy central `globalStorageUri/importlens.redb` file when present.

**FR-026a** (High) - The `redb` database must include a metadata table containing a `schema_version` integer. The current schema version is `6`. On startup, the daemon must read this value before loading cache entries. If `schema_version` is missing or does not match the version expected by the current daemon binary, the daemon must delete the existing database file, create a fresh empty database with the current schema version, and log a warning. This ensures forward compatibility across daemon upgrades (including the redb v3→v4 major version migration and protocol-result shape changes).

**FR-026b** (Medium) - The daemon must track recency as a process-global monotonic sequence (`last_seq`) stored inside each cache entry: interactive hits promote the in-memory sequence, bulk/prewarm reads do not (scan resistance), and promoted sequences are re-persisted during the shutdown/recycle flush so recency survives restarts. There is no separate recents table - removing an entry removes its recency, so dangling recency rows are structurally impossible. Startup preload and post-hello prewarm select up to the 20 highest-sequence entries by reading each stored value's fixed sequence prefix. On handshake completion, the daemon must prewarm those entries after resolving them from the active workspace dependency tree.

**FR-026c** (Critical) - **A non-durable outcome must never enter a durable store.** Durable means: the L1 memory and L2 disk import caches, the L1 aggregate file-size cache, every build memo, the extension's persisted histories, **and any pass/fail verdict** (FR-032a).

  **The gate is an allowlist over the failure stage, never `error` (FR-026d), and never a denylist.** A result may be written down only if its stage is a property of the package's **bytes** — the cache is keyed by those bytes' fingerprints, so such a fact expires exactly when it would change. A denylist of the three transient engine stages is not sufficient and was not correct: `entry_metadata` is a bare `fs::metadata` failure (a locked file, a permission blip, a drive that blinked), which is transient in fact while belonging to no engine-stage list, and under a denylist it was cached and expired only when the package's manifest changed. The allowlist refuses every stage nobody has classified, so the cost of forgetting is a rebuild, never a durable wrong answer (`pipeline::stage::may_enter_a_durable_store`).

  Three shapes are refused. A result the engine could not measure under a transient stage (`timeout` / `panic` / `engine_gone`). A result whose stage is a condition of the **machine** rather than the package (`entry_metadata`, `compression`). And a *successful* measurement whose full-package comparison build hit a transient stage: its sizes are real, but its `truly_treeshakeable: false` is fabricated by the same accident, and caching it marks a healthy package "not tree-shakeable" for a whole cache generation.

  A **deterministic** outcome (`parse`, `link`, `resolve`, `output_shape`, `module_graph_limit`, `missing_export`, `oversized_entry`, an unusable manifest, a minification failure) **is** cached, with or without a size. Refusing it would re-enter the engine for a broken package on every analysis, forever, on one of only two permits.

  **The gate must live IN each store, not in a predicate its callers are expected to call.** `ImportCache::insert*`, `DiskCache::insert*` and `FileSizeCache::insert` each ask the question themselves and keep nothing when the answer is no. A predicate beside the store is exactly the shape of this defect — something a future caller can forget, with nothing failing when they do — and it is what let the seventh instance through. A **property test must quantify over the stores**, feeding each one a real non-durable result and asserting it kept nothing (`service.rs`, `extension/test/analysis/transience.test.ts`).

  **And on the READ, not only the write.** A write-side gate protects a store from what it is handed today; it says nothing about what is already inside it. L2 outlives the process, so a row a build wrote before the gate existed is on real users' disks right now, and it would be decoded, served as a cache hit, and re-promoted into L1 forever. `DiskCache`'s read path therefore applies the same gate and **evicts** what it refuses. The cost is one rebuild.

  The four build-derived stores — the full-package and export-list memos, the build memo they share, and the dependency-path index — need no gate of their own, and must not acquire one: their only input is a successful build's artifact, so a failure cannot reach them. This is enforced structurally, not by review (`scripts/test/result-model-guards.test.mjs`).

  **The static guard's coverage is stated as a number, the number is not "all", and the number is measured rather than claimed.** `scripts/test/result-model-guards.test.mjs` bans the negative-`error` usability check by scanning source text, and a regex is not a parser: it catches **18 of the 24** spellings in the corpus committed beside it (`BANNED_SPELLINGS`). The count is computed by a test, not typed in, so widening or weakening any matcher fails the build until the claim moves with it.

  **The corpus is not the universe, and this is the part that must not be glossed.** A corpus assembled from spellings a matcher already catches measures nothing. Successive versions of that guard scored 6, then 14, then 15 of the reviewer's original 18 — while an *independently planted* set of 13 fresh spellings scored **3 of 13**. Folding the best of those plants back in, and closing the Rust pattern-match and combinator holes they exposed (`if let Some(_) = x.error`, `match x.error`, `x.error.map_or(…)`), brings the independent set to **6 of 13**: Rust 5 of 5, TypeScript 1 of 8. What still gets through is structural — destructuring or aliasing the field throws the receiver away, a bare `x.error == null` in an expression is indistinguishable from the nine legitimate wire-shape validators in `ipc/client.ts`, and a ternary is how a correct error *reporter* is written too. Telling those apart needs types, not a wider regex.

  **A guard that looks stronger than it is, is worse than none**: it buys false confidence, which is how this defect survived seven rounds. So the guard is a **tripwire for the canonical spellings**, and nothing more is claimed for it. **The rule is enforced at runtime, by the gate inside each store**, over which the property tests above quantify — deriving the entire non-durable stage vocabulary from the allowlist rather than restating it.

  The **aggregate** cache needs two gates more than the stage scan, because a file total can be wrong without any stage having failed *and* wrong without any input being missing.

  * **`incomplete`** — a conservative per-import sum (FR-024a) that skipped a still-`loading` import carries `error: None` and no failing stage at all. The daemon must flag as `incomplete` any sum missing a contributor's bytes, for any reason, including a deterministic one and a package that is **not installed** (FR-024a — a path alias that resolves to first-party source is not one of those, and must flag nothing), and must lift the missing import's stage into the aggregate's diagnostics. The sum must be structurally incapable of adding the bytes and dropping that signal.
  * **`degraded`** — the file's own combined build failed, so the totals are an un-deduplicated per-import sum: a different quantity, and an over-count (FR-024a). This is refused **separately**, and it must be, because it is exactly the state in which every contributor is Measured, `incomplete` is `false` and `error` is `None`. A transient-stage scan catches only half of it: a *deterministic* combined-build failure carries a perfectly durable stage.

  **"Any durable store" includes the extension's, and those are worse.** The import-cost history and the bundle-impact history have no TTL, no cache generation, and no Clear Caches command behind them. They keep **one row per identity**, so a bad row does not go stale — it *replaces that import's real baseline permanently*, and every later trend is computed against a number that never happened.

  **The gate lives in the STORE, not in the row's constructor.** Gating only the constructor leaves the store takeable: `ImportCostHistoryItem` is five sizes and an identity, so once one exists nothing downstream can re-derive how it was measured, and a row built by hand went straight into `globalState`. `recordImportCostHistory` and `recordBundleImpactHistory` therefore take the **raw daemon output** — the analysis states, the file-size response — and build the rows themselves. Neither will accept a row. The property test must feed the **stores**, not the constructors.

  * an `ImportResult` may be recorded only if it **has a size** and carries no transient stage. Both conditions are load-bearing: the first catches the Unmeasured shapes, and the second catches the degraded-comparison shape, which has real sizes (FR-026d — the two are NOT mutually exclusive);
  * a file total may be recorded only if it is `error`-free, not `incomplete`, not `degraded`, and carries no transient stage of its own. Both flags are on the wire (`FileSizeDocumentResponse`, and the legacy `FileSizeResponse`) because the client cannot derive either: a still-`loading` import leaves no failing stage behind, and a failed combined build leaves every contributor looking perfect;
  * the bundle-impact row's `importCount` comes from `states`, never `imports`. `incomplete` guards the *bytes*, not the *count* — a still-`loading` import is exactly one the count must include, and `imports` omits it.

  **One rule, three processes, one drift check.** The daemon (`FileSizeComputation::is_cacheable`), the extension (`isDurableFileSize`) and the CLI (`isUsableFileSize`) each state this gate, because the CLI ships standalone and can import neither of the others — the same forced duplication as `engine::stage::is_transient`. They must consult the same fields, and a drift check must fail when one of them falls behind (`scripts/test/file-size-usability-coordination.test.mjs`). `degraded` was added to the first two and not the third, and the third is the one that issues CI verdicts.

  A total that fails this gate is still **shown** (a floor beats a blank, FR-024a) — labelled as an estimate (the status bar prefixes it `~`), and with no delta against the previous run, since comparing an honest number to a floor invents a regression. It is simply never written down.

**FR-026d** (Critical) - **A size exists if and only if a build succeeded.** Every import analysis is in exactly one of three states, and the state must be legible from the result's *type*, not from a convention a reader has to know (ADR-0006):

  * **Measured** — the five sizes are present. A build succeeded.
  * **Loading** — no size *yet*; a build is in flight and the response did not wait for it. This is not an `ImportResult` at all: it is an analysis item with `status: "loading"` and no result.
  * **Unmeasured** — no size, *ever*. The build could not answer, and the result carries the **stage** that could not answer it.

  There is no fabricated state. The daemon must not size a package from the bytes of its directory on disk, from its entry file alone, or from a zero. All three were shipped and all three are deleted: the "brotli size" of an unreadable-manifest import was an uncompressed directory including its tests and source maps, and the size of a failed build was one file out of a graph.

  Three rules follow, and they are what the code must enforce:

  1. **The question a consumer asks is "is there a size?"** — an `Option<u64>` in the daemon, `number | null` on the wire and in the extension, enforced by the compiler on both sides. **Never "is there an error?".** That negative check is the defect itself: it was written in six different places and every one of them waved through a result carrying `error: None` *plus* a fabricated size. A missing size must never be defaulted to zero — `0 B` reads as "this import is free", and `unwrap_or_default()` in the workspace report printed exactly that. Both forms are banned by a guard test over every size-consuming file.
  2. **Unmeasured splits by cause.** *Deterministic* (`parse`, `link`, `missing_export`, `ambiguous_export`, `output_shape`, `module_graph_limit`, `oversized_entry`, `package_manifest`, `entry_resolution`, `package_validation`, `package_resolution`, `minify`) is a property of the package's bytes: same input, same outcome, so it may be cached. *Transient* (`panic`, `timeout`, `engine_gone`) is a property of this moment's scheduling and says nothing at all about the package. A third group is neither: a condition of the **machine** (`entry_metadata`, `compression`) — an unclassified stage joins it by default. Only the first group may enter a durable store (FR-026c). The transient list lives in `engine::stage::is_transient` and is mirrored — under a drift check — in the extension and the CLI, which cannot import it; the durability allowlist over the whole vocabulary lives in `pipeline::stage`.

     *This is a distinction about caching, not about totals.* A deterministic failure is cached as a per-import fact **and** still makes its file's total a floor (FR-024a). Both are true; conflating them was the seventh instance.
  3. **The FABRICATED state must be unrepresentable**, not merely tested for. A fabricated state is a size no build produced. In the daemon this is enforced by construction: the size fields are private and the only two constructors are `ImportResult::measured(specifier, sizes)` and `ImportResult::unmeasured(specifier, stage, …)` — so a failing path cannot reach for a size, because the constructor that takes one is the one that does not take a stage.

     **A size together with a transient stage is a different thing, and it is NOT unrepresentable — it is a real state.** A build can succeed and produce honest sizes while the *secondary* full-package comparison build times out, leaving `truly_treeshakeable` decided by a scheduling accident. That result must exist (the user is owed the fact, and the caches must be able to see it), and no type can prevent it in any case: `diagnostics` is an open list whose `stage` is a string. The requirement previously asserted both that this shape was unrepresentable and, in FR-026c, that a gate must catch it — which cannot both be true. It is representable, it is named (`ImportResult::is_transient`), and the **runtime** gate that keeps it out of every store is airtight instead: it lives inside each store (FR-026c), not in a predicate a caller must remember.

  Accepted consequence: coverage drops. An import whose manifest cannot be parsed, whose entry exceeds the module source limit, or whose build fails shows **"Size unavailable"** rather than a number. A wrong number is worse than no number, because a user cannot tell it is wrong.

  Wire note: the five size fields are plain `Option<u64>` with **no** `skip_serializing_if`. The L2 disk encoding is positional msgpack (`rmp_serde::to_vec`) and these fields sit mid-struct, so skipping one shortens the array and every field after it decodes off by one. This is guarded by a round-trip test, and `CURRENT_SCHEMA_VERSION` was bumped to 8 — not because an old entry fails to decode, but because it decodes *perfectly*, resurrecting every fabricated size a previous daemon wrote as a genuine measurement.

  The transient stage vocabulary is one list, owned by the daemon (`stage::is_transient`) and mirrored in the extension because the two cannot share a source; a drift check fails the build if they disagree.

**FR-027** (High) - The TypeScript extension host must watch `node_modules` for package version changes using VS Code's native `vscode.workspace.createFileSystemWatcher` API with two glob patterns: `**/node_modules/*/package.json` for regular packages and `**/node_modules/@*/*/package.json` for scoped packages (e.g. `@babel/core`). Both watchers must be registered at activation and disposed on extension deactivation. The `notify` Rust crate must not be used for this purpose. On Linux, a Rust process watching `node_modules` directly would register one `inotify` file descriptor per directory, which on kernels before 5.11 could rapidly exhaust the system-wide `inotify` limit (`fs.inotify.max_user_watches`, which defaulted to 8,192 prior to kernel 5.11). Since kernel 5.11 (February 2021), the default is dynamically scaled based on available memory (up to 1,048,576 on 64-bit systems with >=128 GB RAM), but the old default persists on older kernels and in constrained containers. Regardless of kernel version, VS Code's file watcher already manages file descriptor budgets safely for all extensions combined, making it the correct abstraction. Watcher events must be debounced into bursts. Empty bursts must be ignored. For 1 through 20 changed `package.json` paths in one burst, the extension host must send a single `NodeModulesChanged` message containing the changed paths; the daemon then resolves package names from those paths and evicts matching cache entries from both `papaya` and `redb`. For entire `node_modules` deletion/replacement, malformed package paths, or more than 20 changed packages in one burst, the extension host or daemon must use `CacheInvalidateAll` semantics and evict all entries from both cache tiers. See Section 10.1 for the `NodeModulesChangedMessage` and `CacheInvalidateAllMessage` schemas.

**FR-027a** (Critical) - The extension host must watch the workspace's **alias-table configs** on the same watcher, the same debounced burst, and the same `NodeModulesChanged` message as FR-027: the glob patterns `**/tsconfig*.json` and `**/jsconfig*.json`. Config paths under `node_modules` must be dropped — they belong to a dependency's own build, the daemon never reads one, and an install would otherwise queue thousands. The changed config paths travel in the message's `tsconfig_paths` field, separately from `package_json_paths`, because they mean a different thing: the daemon must drop the shared resolvers (which memoize the parsed config) and the L1 aggregate cache, and must **not** bump the cache generation or evict a shard — a `tsconfig.json` cannot change what a package *weighs*, and re-verifying every measured import against disk would buy nothing. The 20-path burst limit of FR-027 counts `package.json` paths only: a config edit is a single deliberate keystroke, and collapsing it into `CacheInvalidateAll` would discard every measured package for a change that cannot affect one.

  Without this the daemon read the alias table exactly once — `oxc_resolver` memoizes the parsed config in the resolver's filesystem cache, and only a `node_modules` change ever dropped it. So a developer whose file the daemon reported as a floor, who then applied **the exact repair this SRS prescribes** (mirror the alias into tsconfig `paths`), got no change for the rest of the daemon's life. A remedy a spec prescribes and the code ignores is worse than no remedy: it is the spec lying.

  **What the message still buys, stated exactly.** The alias resolvers now memoize no filesystem fact — they are rebuilt per query, which is what stops a floor being sticky (FR-024a) — so a `paths` **edit** is picked up on the next request with no message at all. What survives a query is the **reachable-config walk**: which projects the workspace's `references` graph reaches. A config that starts *referencing* the project that owns the `paths` therefore stays invisible until this message drops that memo, and the L1 aggregate cache still holds totals computed under the old table. Both are dropped here.

**FR-028** (Medium) - When a user opens or saves a `package.json` file in the workspace, the daemon must pre-calculate and cache the sizes of the default export and the namespace export (`*`) for each dependency listed in that file's `dependencies` and `devDependencies` objects. These two export variants are the most common and cover the majority of real-world import patterns. Pre-warm tasks must run on a dedicated secondary Rayon thread pool configured with half the threads of the primary pool, so that the primary pool remains fully available for real user requests. Because Rayon does not expose OS-level thread priority, reduced pool size is the correct mechanism for deprioritisation. Pre-warm work must stop immediately when foreground analysis or cache-mutating work arrives, including batch, document, package.json, raw-specifier, export-enumeration, file-size, completion, invalidation, cleanup, removal, shutdown, and recycle paths. Prewarm must reuse already-resolved package entries rather than resolving the same package twice.

### 5.6 User Interface

**FR-029** (Critical) - The extension must display size information inline by default through `importLens.display: "inlayHint"` and `importLens.inlineRenderer: "colored"`. The colored inline renderer is the default because it can apply muted, segmented inline annotation colors directly beside each import. Native VS Code inlay hints remain available through `importLens.inlineRenderer: "native"` for users who prioritize screen-reader-accessible editor integration over per-part inline colors. End-of-line text decorations remain available via `importLens.display: "standard"` or `importLens.display: "verbose"` for users who prefer line-end annotations.

**FR-030** (Critical) - The display format must be configurable via `importLens.display` with four options:
- `minimal`: `1.5 kB` (primary compression format only)
- `standard`: `1.5 kB br · 5.3 kB min` (primary compression size and minified size)
- `verbose`: `1.5 kB br · 1.8 kB gz · 1.6 kB zstd · 5.3 kB min` (all three formats)
- `inlayHint`: Displays the primary compression size as an inline hint at the end of the import statement (e.g., after the semicolon). Rendering is selected by `importLens.inlineRenderer`: `native` uses the VS Code Inlay Hints API with segmented label parts, while `colored` uses decoration-backed inline text with muted per-segment tones.

**FR-031** (High) - When `side_effects: true`, `is_cjs: true`, or `truly_treeshakeable: false`, the extension must warn users that the shown size may be conservative. Inline labels may use short module-type tags such as `CJS`, `server`, and `types only` as separate muted suffix segments, but must not append the literal word `conservative`. Conservative-sizing context belongs in hover details, show-import-details, and the workspace report. The literal word `approximate` must not appear in inline size labels. Low-confidence size labels must use a leading `~`, for example `~1.6 kB br`. Medium- and high-confidence labels must not use `~`; confidence is conveyed through hover, report, diagnostic details, and inline size-tone colors on surfaces that support per-segment colors.

**FR-031a** (Medium) - When an import is detected from Astro frontmatter, the extension must label the displayed size with `server` and include the runtime in the hover tooltip so users do not confuse server-only dependency cost with client bundle cost.

**FR-031b** (Medium) - When the active file is tracked by Git and an import statement overlaps an added or modified line in the working-tree diff against `HEAD`, the extension must append a positive import-cost delta label based on the current import's Brotli bytes, for example `+2.1 kB br`. Deleted imports have no current editor range and are out of scope for inline labels.

**FR-031c** (Medium) - The extension must persist a bounded per-import size history in VS Code global storage. When a current import result differs from the most recent stored entry for the same import identity, the hover tooltip must include a trend note showing the previous Brotli size, current Brotli size, and signed delta.

**FR-031d** (Medium) - When multiple imports in the same file share module paths reported by `module_breakdown` and `shared_bytes`, the extension must add hover insight text naming up to three shared module basenames and the other specifiers that include them. If the daemon reports `shared_bytes` but the shared modules are outside the public top-module breakdown, the hover must still explain that shared bytes exist.

**FR-031e** (Medium) - When the parser detects a star re-export (`export * from "package"`), the extension must surface a barrel-boundary insight. The inline label may append `barrel` as a separate muted suffix segment, and the hover must explain that the broad re-export can prevent precise named-export tree-shaking.

**FR-031f** (High) - Inline editor annotations for imports and `package.json` dependency hints must read as metadata rather than source code. Import inline hints use `gitDecoration.addedResourceForeground` for high-confidence sizes (matching `package.json` `latest` suffix green), `gitDecoration.modifiedResourceForeground` / `gitDecoration.deletedResourceForeground` for medium/low confidence sizes, `editorCodeLens.foreground` for neutral loading labels, `descriptionForeground` for module tags, and other `gitDecoration.*` tokens for insight suffixes with italic styling. `package.json` dependency hints use `descriptionForeground` for primary labels and `gitDecoration.addedResourceForeground` / `gitDecoration.modifiedResourceForeground` for registry suffixes. Colored inline hints must render the primary size and suffix segments in deterministic order using fixed slot-ordered decoration layers (`primary`, then `suffix0`..`suffixN`): VS Code chains `after` pseudo-text from different `TextEditorDecorationType` instances at a shared anchor in `setDecorations` call order, so batching segments by theme color reverses or merges segment text. Per-segment colors are set on each decoration option's `renderOptions.after.color`. Report rows, treemap legends, and full detail surfaces retain the semantic `charts.*` confidence color mapping for data visualization. Full detail surfaces must emphasize key fields with Markdown structure: bold package name, bold selected compression size, a confidence badge or label, reason list, side-effect/CommonJS/tree-shakeability status, and a diagnostics command link when diagnostics are available.

**FR-032** (High) - The extension must display a loading indicator next to imports that are currently being computed (cache miss in progress).

**FR-033** (High) - The extension must provide a status bar item showing the daemon's current state: `Import Lens: Ready`, `Import Lens: Computing...`, or `Import Lens: Unavailable`.

**FR-034** (High) - Changing the `importLens.compression` setting must immediately update all currently visible inline decorations to reflect the new format selection without requiring a file change or editor reload.

**FR-035** (Medium) - The extension must provide cache management commands. `Import Lens: Clear Current Project Cache` must remove only the cache shard for the active project's analysis root, `Import Lens: Clear All Caches` must remove every Import Lens project cache shard and any leftover legacy central cache file, and `Import Lens: Manage Cache` must show a Quick Pick UI with cache status, cleanup, current-project removal, all-cache removal, and per-project inspection/removal actions. Destructive actions must ask for confirmation and trigger a fresh computation for visible documents after successful removal.

**FR-036** (Medium) - The extension must provide a command `Import Lens: Show Report` that opens a webview panel listing all imports in the workspace along with their sizes, sorted by brotli size descending. The report must include workspace summary metrics, duplicate import aggregation, duplicate/vendored module insights, a static SVG treemap sized by Brotli bytes, confidence legend colors, and a static shared-module table so users can quickly identify dominant dependencies without running scripts in the webview.

**FR-036a** (Medium) - Report rows and hover tooltips must surface file-level sharing information when the daemon returns it. `ImportResult.module_breakdown` contains the top 10 module contributors for an import. `ImportResult.shared_bytes` contains the number of raw module bytes shared with at least one other import in the same file. The report must expose both the top contributors and shared-byte value without changing the inline decoration format.

**FR-036c** (Medium) - Report rows, hover tooltips, and copied diagnostics must expose `ImportResult.confidence` and `ImportResult.confidence_reasons`. Low- and medium-confidence rows must be countable in the report summary and must include the reasons in the row warning text.

**FR-036d** (Medium) - The extension must provide named-import member completions for existing ESM import clauses. When the cursor is inside `import { ... } from "specifier"`, the completion provider must request `EnumerateExportsRequest` from the daemon and offer cached named exports from the resolved graph. Completion requests must be best-effort and must fail silently in degraded mode.

**FR-036e** (Medium) - The extension must provide a command `Import Lens: Show Current File Size` that sends a `FileSizeDocumentRequest` for the active file, receives a deduplicated file-level total, displays the selected compression summary, and records the measurement in bundle impact history — only when the total is a measurement of the file rather than a floor (FR-026c). The command must work for supported loose files using the same analysis-root derivation as FR-006c.

  **The file's imports are its `states`, never its `imports`.** This read is streamed like any other (it does not force-fresh), so `imports` carries only the imports the daemon has already MEASURED, while `states` carries every one it detected. On a **cold** document — the one the user just opened, and the likeliest one to run this command on — `imports` is therefore empty while the file's own totals, which come from the combined build and not from the per-import measurements, are perfectly real. Gating the report on `imports` told the user the file had "no resolvable package imports" for a file the daemon had sized correctly, and counting them reported "0 imports" for a file that had three. Only a document whose `states` are empty has nothing to report. The same trap is called out for the status bar in the FR-024a floor rules, and it is the same one, in the same shape, one command away.

  `skipped` in the summary means *the daemon could not size this import* (`missing`/`unavailable`), never *it has not sized it yet*: an import still building is not skipped — it is why the total is flagged as an estimate.

**FR-036f** (Medium) - The extension must provide a command `Import Lens: Show Bundle Impact History` that reads recent current-file measurements from VS Code global storage and opens a script-free static SVG history panel with timestamp, file path, import count, and byte details.

**FR-036g** (Medium) - The extension must provide CodeActions for imports whose current result is CommonJS, side-effectful, or not truly tree-shakeable. These actions must allow users to inspect existing Import Lens details or copy diagnostics. They must not automatically rewrite user source.

**FR-036h** (Medium) - For namespace imports whose result is not truly tree-shakeable, the extension must offer a CodeAction that enumerates named exports through `EnumerateExportsRequest`, lets the user select one or more export names, and copies a candidate named import statement to the clipboard. The action must not rewrite source automatically because namespace member usage requires semantic transformation outside the current static import-cost scope.

**FR-036i** (Medium) - The extension must support configured bundle budgets for per-import and per-file Brotli thresholds. Budget violations must appear as VS Code diagnostics, must be visible in inline/hover/report text, and must be counted in report summaries.

**FR-032a** (Critical) - **No verdict from a floor, and a gate that cannot measure must never report success.** A budget is judged against a size, so an import that has none is **not evaluated** — neither "pass" nor "fail". This applies to every surface that reaches a verdict: the Problems-panel diagnostics, the workspace report's violation counters, and `importlens check`.

  Silence must therefore be read correctly on each of them. In the editor, an unmeasured import simply produces no budget warning, and shows "Size unavailable" beside it so the user can see why. That is acceptable in an editor, where the user is looking at the file. It is **not** acceptable in CI, where nobody is: `importlens check` must exit **non-zero with a distinct code (3) and a distinct message** when it could not measure what it was asked to judge — because a file's total is a floor (`incomplete`), because the file's own combined build failed (`degraded`), or because an import failed transiently. A budget violation remains exit 1. Confusing the two in either direction is a failure of this requirement.

  **And no verdict from an over-count either.** A `degraded` total (FR-024a) is a sum of per-import costs, not a File Cost: it is *larger* than the file, so it cannot produce a false pass — but it can produce a false **FAIL**, and invariant 5 forbids both. A budget judged against a number the file never had is neither passed nor failed. The gate must apply the same predicate the daemon's L1 aggregate cache and the extension's persisted history apply (`isUsableFileSize`, held in lockstep with them by a drift check, FR-026c). It did not: `degraded` reached the CLI on the wire and the CLI read `brotli_bytes` straight past it.

  **The gate belongs to the verdict, not to its caller.** `runImportLensCheck` applies `isUsableFileSize` itself, to the raw response fields, rather than trusting the injected `analyzeFile` to have decided. A pass/fail verdict is a durable store, and a store that trusts its caller to have asked is a store that will eventually not be asked.

  **One unmeasurable file is one unmeasurable file.** A file whose aggregate failed outright (`error`: nothing could be sized at all) is reported as "could not measure" and the run **continues** to the next changed file. It must not throw: throwing abandons every other file's budget mid-run and exits **2**, which means "the CLI broke" — not the exit 3 this requirement mandates, and CI cannot tell the two apart.

  The old gate did the worst available thing. It filtered imports with `!item.error` and read the file total while discarding `incomplete`, so a build that merely timed out dropped its import from the comparison and CI went **green** — and the regression merged. A flaky agent must be diagnosable and must never be mistaken for either a pass or a regression.

  **A floor blocks the file's verdict whatever made it one.** The cause changes the remedy, not the ruling: a transient stage may well pass on a re-run, while a deterministic one is a package this build cannot measure and never will be — but in both cases the file's real total is larger than the number in hand by an unknown amount, and "under budget" is not a fact the run established. The exit-3 message must say which, so a flaky agent and a genuinely unmeasurable package are not confused with each other, and neither is confused with a regression.

  A file whose total is a floor is **not evaluated at all** — neither its per-file budget nor the per-import budgets of the imports inside it. That is coverage the gate gives up, and gives up on purpose: it is a loss of *detection*, never a false pass, and the run exits non-zero so nothing merges behind it.

  What it must not do is **hide** what it did find. A budget violation confirmed in a *different* file, one that was measured, must still be printed even though "could not measure" outranks it in the exit code. It previously was not: the exit code won and took the finding with it, and a deterministically-unmeasurable import in any changed file was enough to silence a real regression in every other.

  Accepted consequence: a repository with an import this daemon cannot measure gets exit 3 from `importlens check` until that import is measurable, removed, or its file stops changing. Exit 3 is not a regression and does not claim to be one. The alternative — the previous behaviour — is a silent green on a file whose size nobody knows, which is the one outcome FR-026d calls worse than any other, because it merges the regression.

**FR-036j** (Medium) - The repository must provide an `importlens check` CLI path that analyzes files changed by `git diff` and exits non-zero when configured budgets are violated (exit 1) or when it could not measure them (exit 3, FR-032a). The CLI must fail clearly for malformed budget configuration and must not require VS Code to be running.

**FR-036k** (Medium) - The extension must offer curated import substitution suggestions through CodeActions using only a local mapping file. Suggestions may copy or show alternatives and size context, but must not rewrite source automatically.

**FR-036l** (Medium) - When `importLens.enableRegistryHints` is enabled, the daemon performs all npm registry fetches for registry-based hints via the protocol v7 `RefreshRegistryHintsRequest`. The setting must default to `true`; the extension host must never call the npm registry directly and registry work must never block size computation or package.json analysis. The extension host only requests refreshes and renders the returned results. The controller must render cached registry hints immediately on `package.json` open, including stale successful hints when no fresh value is available, then request a `refresh_stale` mode refresh for missing or stale metadata automatically in the background. Automatic and manual refreshes must use the daemon's shared refresh path: bounded concurrency, shared interval rate limiting, package-level in-flight de-duplication, short per-request timeouts, hard retry limits, `Retry-After` handling for npm `429` responses, and cached retry windows after transient failures. Positive, negative, and transient-error states are cached in the daemon's centralized package metadata cache under the extension-managed daemon cache base. Registry failures must fail silently without affecting size computation. Package dependency hovers must expose a trusted refresh action that sends a `force_refresh` mode request for that one package only while still using the daemon's shared concurrency/rate-limit path. Dependency summary hovers must expose a trusted refresh action that sends a `force_refresh` mode request for all dependencies represented by that summary, again using the daemon's shared concurrency/rate-limit path.

**FR-036m** (Medium) - When a `package.json` file is open, the extension must provide compact dependency-cost end-of-line decorations for dependency blocks using local package resolution and daemon-owned size requests. Rendering must read from cached package.json analysis state rather than starting daemon, registry, or resolver work from a decoration refresh handler. The package.json controller must request daemon streaming so dependency rows appear as soon as entries are parsed and package resolution completes, then update individual rows incrementally as package size results and registry hints arrive. Each dependency entry may show its measured compressed size, `not installed`, `checking...`, `unavailable`, or a deprecation suffix. A daemon timeout or failure after partial responses must preserve completed states and mark only remaining `checking...` rows unavailable. Each dependency block should also expose a compact measured/total summary when analysis state is available. Dependency hovers must show the individual registry fetched time when available. Summary hovers must show the oldest registry fetched time across represented dependencies, or state that some registry info has not been fetched yet. Inline decorations must use independent primary and suffix colors: primary text (size, `types only`, `checking...`, or `unavailable`) uses `descriptionForeground` except `unavailable`, which uses `list.errorForeground`; registry suffixes (`latest`, `update`, `install`) use `gitDecoration.addedResourceForeground` and `gitDecoration.modifiedResourceForeground` respectively, rendered in italic, and may appear even when sizing is unavailable. Section summaries use muted foreground only.

**FR-036n** (Medium) - The extension must provide `Import Lens: Compare Imports`, allowing users to compare two package specifiers side by side using the same local daemon sizing path as normal import analysis.

**FR-036o** (Medium) - The extension must provide a static SVG history panel generated from existing bundle impact history data. The webview must keep scripts disabled.

**FR-036p** (Medium) - The extension must support `.importlensignore` using gitignore-style package, path, and import-pattern rules to suppress analysis and decorations for matching imports.

**FR-036q** (High) - The daemon must own workspace report source scanning and report data aggregation. The extension host may request a workspace report for a workspace root and render the returned report model, but it must not enumerate/open every source file or rebuild duplicate-import/shared-module summaries itself. The request carries the editor's current report budgets so per-import and per-file budget warnings remain user-configurable while the aggregation stays daemon-owned. The daemon scan is read-only, limited to supported source extensions, and skips `node_modules`, `dist`, `build`, `out`, and `coverage` directories.

### 5.7 Configuration

**FR-037** (Critical) - The extension must expose the following user-configurable settings via the VS Code settings panel:

| Setting key                         | Type    | Default     | Description                                                                                                    |
| ----------------------------------- | ------- | ----------- | -------------------------------------------------------------------------------------------------------------- |
| `importLens.enabled`                | boolean | `true`      | Toggle the extension on or off                                                                                 |
| `importLens.display`                | enum    | `inlayHint` | Display format: `minimal`, `standard`, `verbose`, or `inlayHint`                                               |
| `importLens.inlineRenderer`         | enum    | `colored`   | Inline renderer for `display: "inlayHint"`: `colored` decoration-backed hints or `native` VS Code inlay hints  |
| `importLens.compression`            | enum    | `brotli`    | Primary compression format shown in minimal and standard modes. Options: `brotli`, `gzip`, `zstd`, `all`       |
| `importLens.debounceMs`             | number  | `300`       | Milliseconds to wait after the last keystroke before sending a request                                         |
| `importLens.showWarnings`           | boolean | `true`      | Show warning indicator for non-tree-shakeable imports                                                          |
| `importLens.useCodeLens`            | boolean | `false`     | Use code lens above the line instead of end-of-line decorations                                                |
| `importLens.enableDiskCache`        | boolean | `true`      | Persist computed sizes to disk via redb across editor restarts                                                 |
| `importLens.cacheMaxSizeMB`         | number  | `512`       | Global disk-byte budget across all project cache shards; least-recently-used entries are evicted when exceeded |
| `importLens.registryCacheMaxSizeMB` | number  | `32`        | Byte budget for the shared npm registry metadata cache; oldest entries are evicted when exceeded               |
| `importLens.budgets`                | object  | `{}`        | Optional per-import and per-file Brotli thresholds for diagnostics and CLI checks                              |
| `importLens.enableRegistryHints`    | boolean | `true`      | Enable short-timeout npm metadata hints cached in the daemon's centralized package metadata cache              |
| `importLens.verboseRegistryLogging` | boolean | `false`     | Log per-package registry refresh outcomes for diagnostics                                                      |
| `importLens.logLevel`               | enum    | `info`      | Logging verbosity for the Import Lens output channel. Options: `error`, `warn`, `info`, `debug`                 |

### 5.8 Daemon Lifecycle

**FR-038** (High) - On extension deactivation (or VS Code window close), the extension host must send a `Shutdown` message over the IPC socket. On receiving this message, the daemon must:
1. Stop accepting new requests.
2. Cancel active prewarm work.
3. Flush pending recency touches to `redb` without performing a full `papaya`-to-`redb` rewrite.
4. Close/drop the `redb` database handles.
5. Remove the Unix socket file (macOS/Linux) or release the named pipe (Windows).
6. Exit the process cleanly within 5 seconds.

If the daemon closes the IPC socket cleanly before the 5-second timeout elapses, the extension host must treat that as a successful exit and skip the escalation sequence below. If the daemon does not exit within 5 seconds of the `Shutdown` message, the extension host must send `SIGTERM` (Unix) or call `TerminateProcess` (Windows) to request termination. If the daemon still has not exited after an additional 2 seconds following the `SIGTERM`, the extension host must send `SIGKILL` (Unix) to forcefully terminate it. (`SIGTERM` can be caught or ignored by the process; `SIGKILL` cannot.) On Windows, `TerminateProcess` is already unconditional and no second step is needed.

### 5.9 Diagnostics and Logging

**FR-039a** (Medium) - When `importLens.useCodeLens` is set to `true`, the extension must register a `CodeLensProvider` for the relevant language selectors and render one `CodeLens` per import line, positioned on the line above the import statement. The lens must display the primary compression size and, when clicked, open the full size breakdown in a hover-style `MarkdownString` notification. The `useCodeLens` setting is independent of `importLens.display`; if `inlayHint` display mode is active simultaneously, inline hint rendering takes precedence and the `CodeLensProvider` must not be registered. The `CodeLens` approach is noted as less space-efficient than inline hints (see D-011) but is retained as an option for users who prefer it.

**FR-039** (High) - When `importLens.display` is set to `inlayHint` and `importLens.inlineRenderer` is `native`, the extension must register an `InlayHintsProvider` with VS Code for the relevant language selectors. The provider must return one `InlayHint` per import line, positioned at the end of the import statement (e.g., after the semicolon), with `kind` set to `undefined` (no `InlayHintKind`) and `paddingLeft` enabled so the hint does not visually run into the code. Import sizes are not parameters or types; using `InlayHintKind.Parameter` or `InlayHintKind.Type` would apply the wrong theme colours (`editorInlayHint.parameterForeground` or `editorInlayHint.typeForeground` respectively). An `undefined` kind falls through to the generic `editorInlayHint.foreground`/`editorInlayHint.background`, which theme authors expect for custom inlay hints. Each `InlayHint` label must be constructed as an array of `InlayHintLabelPart` segments (primary size plus suffix labels) to allow interactivity, specifically a `command` on segments that triggers `importLens.showImportDetails` when clicked. Each label part must set `tooltip` to a `MarkdownString` containing the full size breakdown (raw bytes, minified bytes, all three compressed sizes, `side_effects` status, `is_cjs` indicator, runtime, confidence, and any analysis insights from FR-031b through FR-031f). Native inlay hints do not support per-part theme colors; segmented structure improves readability while colors remain theme-unified. When a size is unavailable, the tooltip must show a compact unavailable message and a trusted `Copy diagnostics` command link instead of rendering raw daemon logs inline.

**FR-039d** (High) - When `importLens.display` is set to `inlayHint` and `importLens.inlineRenderer` is `colored`, the extension must render segmented decoration-backed inline hints using the shared inline-hint pipeline also used by `package.json` dependency annotations. Hints must anchor at the end of the import statement (`statementRange.end`, e.g. after the semicolon). The primary size and suffix labels (module tags, git deltas, barrel/budget insights) must use soft semantic editor annotation theme tokens with italic styling. Decoration options must include `hoverMessage` with the same detailed `MarkdownString` used by native inlay hints so hovering the size label shows the full breakdown. The extension must also register a source-range hover provider scoped exclusively to the import specifier string (e.g. `"lodash-es"`) for the same tooltip. This specifier hover must remain tightly scoped so it does not conflict with TypeScript's language-service hover when the user inspects named import bindings. Native inlay mode must continue to rely on `InlayHint.tooltip` on each label part.

**FR-039b** (Medium) - The extension must include a note in its README and marketplace description that `importLens.inlineRenderer: "colored"` is the default for muted segmented inline feedback, while `importLens.inlineRenderer: "native"` is preferred for screen-reader accessibility. End-of-line and decoration-backed inline renderers are not exposed to VS Code's accessibility APIs in the same way as native inlay hints. The native inlay-hint renderer uses the VS Code Inlay Hints API, which is part of the document model and is screen-reader-accessible. The status bar item (FR-033) must always reflect the current operating tier regardless of display mode, as it is accessible to screen readers.

**FR-039c** (High) - The extension output channel must avoid noisy warning duplication. Fallback details for successful low-confidence results belong in diagnostics, hover, report, copied diagnostics, and debug logs. Warning-level output should be reserved for no usable result, daemon/IPC failure, protocol failure, startup failure, or a final unresolved analysis failure, and each `(request_id, specifier, error)` tuple should be logged once. Diagnostic detail for successful low-confidence results should be logged at debug once per `(request_id, specifier)`. See [`docs/logging-policy.md`](logging-policy.md).

**FR-040** (High) - The extension must create a VS Code `OutputChannel` named `Import Lens` for structured diagnostic logging. Log messages must include ISO 8601 timestamps, a severity level, and may include optional component and correlation context (for example `[listener]`, `req=42`). The verbosity is controlled by the `importLens.logLevel` setting. The default level must be `info`, and opening the output channel must write a current log-level breadcrumb even if the configured level would otherwise filter normal lifecycle logs. See [`docs/logging-policy.md`](logging-policy.md).

**FR-041** (High) - The extension must provide a command `Import Lens: Show Logs` that focuses the `Import Lens` output channel in the VS Code panel. This command must be available from the Command Palette at all times, regardless of the extension's current operating tier.

**FR-041a** (High) - The extension must provide a trusted hover command link and registered command `Import Lens: Copy Import Diagnostics`. When invoked from a failed import hover, it must copy the full `ImportResult.error` and `ImportResult.diagnostics` payload for that package to the clipboard. The hover must not display those raw diagnostics directly.

---

## 6. Error Handling and Edge Cases

The system must handle all failure conditions gracefully. No error scenario may produce an uncaught exception in the extension host or a visible error dialog unless explicitly noted below.

| Scenario                                                            | Required Behaviour                                                                                                                                                                                                                                                                                            |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Package not installed in node_modules                               | Display a subtle "Package not found" decoration on that import line. Do not send the import to the daemon. Do not display an error dialog.                                                                                                                                                                    |
| Corrupted, malformed, or versionless `package.json` in node_modules | Report the import **Unmeasured** with a `package_manifest` stage (FR-026d): no size, "Size unavailable" in the UI, and the failure detail in hover/report/diagnostics. Do **not** substitute the package directory's size on disk — that number counted the package's tests, source maps and unused files, uncompressed, and was written to all five size fields, so its reported "brotli size" was an uncompressed directory. |
| Malformed or incomplete import syntax (user is mid-typing)          | Use OXC parser module-record output when recoverable module information is available. Render partial results if a package name can be identified. Suppress decorations silently if no package name can be resolved.                                                                                           |
| Daemon crash                                                        | Detect the process exit, wait 1 second, and restart the daemon. Apply exponential backoff on repeated failures (1s, 2s, 4s, 8s, max 30s). After three crashes within 60 seconds, enter degraded mode and display a status bar warning.                                                                        |
| Socket disconnect without crash                                     | Discard any stale MessagePack payloads currently in the receive buffer. Wait for the next document change event to trigger a fresh request cycle. Do not attempt immediate reconnection to avoid cascading retries on rapid edits.                                                                            |
| IPC frame larger than 32 MiB                                        | Reject the frame before allocation, close or reset the affected request path, and log a diagnostic. The extension must not attempt to decode the oversized payload.                                                                                                                                           |
| Batch-like request sent before `HelloMessage`                       | Do not process the request. `BatchRequest` receives per-import protocol errors; `EnumerateExportsRequest` and `FileSizeRequest` receive protocol error responses. Invalidation and prewarm messages are ignored until hello.                                                                                  |
| Unsupported `HelloMessage.version`                                  | Log the unsupported version on the daemon side, close the connection without accepting subsequent requests from that socket, and rely on the extension host startup/connection recovery path.                                                                                                                 |
| Blocking analysis worker panic or join failure                      | Do not panic the Tokio IPC server. Return a protocol diagnostic response for the affected request when the request shape allows it, and keep the daemon process alive for future requests.                                                                                                                    |
| node_modules folder deleted while extension is running              | The file watcher must detect the deletion. The extension host must send a `CacheInvalidateAll` message (see Section 10.1). The daemon must evict all entries from both `papaya` and `redb`. The extension host must update all affected decorations to "Package not found".                                   |
| redb database corrupted on startup                                  | Log the corruption, delete the corrupted database file, and create a fresh empty database with the current schema. Continue with disk cache enabled when the fresh database can be created; otherwise skip only the persistent tier and keep the in-memory cache for the current session.                     |
| Requested named export missing from a package                       | Return a normal `ImportResult` when partial sizing can continue, include a `missing_export` diagnostic naming the export, and keep the raw diagnostic details in hover-copy output rather than inline UI.                                                                                                     |
| Engine build fails (parse, link, output shape, graph limit)         | Report the import **Unmeasured** under the stage it failed at (FR-026d). Keep successful imports from the same batch intact. Do **not** substitute the entry file's own size: that measures one file out of a graph and reports it as the package's cost.                                                     |
| Package entry file exceeds module graph source limit (20 MiB)       | Report the import **Unmeasured** with an `oversized_entry` stage — deterministic, so it is cached — and expose the diagnostic in hover/report/copy output. No module graph is built, and no size is invented from the entry alone.                                                                            |
| Package ships CSS its JavaScript entry imports                      | Measure the JavaScript graph normally and link the stylesheet as an empty module. Rolldown 1.1.5 cannot bundle CSS (`UNSUPPORTED_FEATURE`, at the link stage), so left to it the **whole build fails** — which, before FR-026d, was invisible because the failure was silently given a fabricated size. Disclose the uncounted stylesheet bytes with an `uncounted_assets` diagnostic naming the files, and hold the result at Medium confidence. |
| Unsupported native platform or missing daemon binary                | Log the missing runtime and enter degraded mode. Display `Import Lens: Unavailable` in the status bar.                                                                                                                                                                                                         |
| Daemon binary hash mismatch (NFR-014a)                              | Refuse to spawn the daemon. Log a security warning to the Import Lens output channel at `error` level. Enter degraded mode and display `Import Lens: Unavailable`. Do not show a user-facing error dialog.                                                                                                      |
| Daemon recycle loop detected (NFR-004b)                             | If more than 5 recycles occurred within any rolling 10-minute window (read from `importlens-recycles.json`), enter degraded mode, log a warning, and display `Import Lens: Unavailable`. Reset counter after a clean 30-minute session with no recycles.                                                       |
| IPC socket path collision (multiple VS Code windows)                | Each window uses a unique socket path via `VSCODE_PID` or UUID at activation (NFR-014b). If the generated path already exists, generate a fresh UUID and retry once before entering degraded mode.                                                                                                            |
| Active file is not in a Git repository or Git diff fails            | Skip working-tree delta insights for that analysis cycle. Do not block import sizing, do not show a user-facing error, and do not require Git to be installed for normal size computation.                                                                                                                    |
| VS Code globalState write fails for history                         | Keep the current import size result visible, log a warning to the Import Lens output channel, and skip only the history/trend update.                                                                                                                                                                          |
| Named export candidate enumeration fails                            | Keep existing tree-shaking CodeActions available, log the daemon or resolution error, and show a compact warning only for the explicit user-triggered action. Do not rewrite source.                                                                                                                          |

---

## 7. Non-Functional Requirements

### 7.1 Performance

**NFR-001** (Critical) - The extension must never block the VS Code extension host main thread. All IPC communication, file system reads, and cache lookups must be fully asynchronous.

**NFR-002** (Critical) - Cache hit response time, measured from the moment the debounce fires to the moment decorations are rendered, must be under 50ms on a mid-range developer machine (equivalent to an Apple M2 or Intel Core i7-12th Gen).

**NFR-003** (Critical) - Cache miss computation time for a single named export from a typical npm package (under 500 kB unpacked) must complete within 500ms on the same reference machine.

**NFR-004** (High) - The Rust daemon must consume no more than 100 MB of resident memory during idle operation with the cache populated. During active computation of a batch of 20 imports, peak memory usage must not exceed 400 MB.

**NFR-004a** (High) - The daemon must implement a silent lifecycle recycle to prevent long-term memory fragmentation. Because developers leave VS Code open for days or weeks, even a well-behaved Rust process accumulates allocator fragmentation over time. The daemon must monitor two conditions and gracefully restart itself when either is met: (a) the daemon has been continuously running for more than 4 hours without an active `BatchRequest` in the last 15 minutes. For the purposes of this timer, only `BatchRequest` processing counts as active; `CacheInvalidate` messages, `HelloMessage` handshake, and pre-warm jobs do not reset the idle timer, or (b) the `papaya` in-memory cache exceeds 200,000 entries (approximately 80 to 100 MB at ~500 bytes per entry, consistent with the 100 MB idle memory limit in NFR-004). A graceful restart must: flush all in-memory `papaya` entries to `redb` before exiting, exit cleanly (no signal kill), and rely on the extension host's existing watchdog (FR-015) to respawn it. The restart must be silent to the user; no status bar change or notification must appear unless the restart fails.

**NFR-004b** (High) - The extension host must detect runaway recycle loops, which would never trigger the crash-based degraded mode in FR-015 because graceful recycles exit with code 0. On each daemon respawn, the extension host must read a recycle counter from a small side file at `<globalStoragePath>/importlens-recycles.json`. The daemon must increment this counter and write the current Unix timestamp before beginning its graceful exit. The extension host must enter degraded mode if the counter shows more than 5 recycles within any rolling 10-minute window, log a warning to the Import Lens output channel, and display `Import Lens: Unavailable` in the status bar. The counter file must be reset to zero on a successful session lasting longer than 30 minutes without a recycle.

**NFR-004c** (High) - When a lifecycle recycle is triggered (NFR-004a), the daemon must abort any in-progress pre-warm jobs (FR-028) immediately before beginning the flush-and-exit sequence. Pre-warm jobs are low-priority background work; they must not delay a recycle. Any pre-warm entries that were computed but not yet written to `papaya` at the time of abort are discarded. They will be recomputed in the next session when the relevant `package.json` is opened again.

**NFR-005** (High) - The daemon must start and be ready to accept connections within 500ms of being spawned.

**NFR-006** (High) - The Node.js extension host memory footprint must remain flat during rapid, continuous typing over a sustained 5-minute period. This must be verifiable via memory profiling as defined in AC-005.

### 7.2 Distribution Size

**NFR-007** (Critical) - The published VSIX for any single platform target must not exceed 20 MB. This constraint applies to every target listed in Section 12.1 individually. This is a hard gate: the CI pipeline must fail the publish step if any VSIX exceeds this size.

### 7.3 Reliability

**NFR-008** (Critical) - A panic or crash in the Rust daemon must not crash VS Code or any other extension. The daemon must run in a separate process.

**NFR-009** (High) - The `redb` persistent cache must be ACID-compliant. A hard shutdown such as power loss or `kill -9` must not corrupt the database. On next startup, the database must be readable and consistent.

**NFR-010** (High) - If the daemon is unavailable, the extension must degrade gracefully. Import statements must still be detected and highlighted normally. The size decorations must simply be absent, not replaced with error text in the editing area.

### 7.4 Security

**NFR-011** (Critical) - The daemon must make no outbound network connections during import size computation, package resolution, module graph construction, tree-shaking, minification, compression, cache lookup, or cache invalidation. The only permitted outbound network path is the registry-hint refresh endpoint, which may call the public npm registry when `importLens.enableRegistryHints` is enabled and a client explicitly requests stale or forced registry refresh. Registry refresh must use centralized package metadata caching, short timeouts, bounded concurrency, shared interval rate limiting, package-level in-flight de-duplication, retry-after handling, hard retry limits, cached retry windows for automatic refresh, manual refresh cache bypass, and stale-cache fallback. Each package failure must be logged and returned as a per-package nullable registry hint result without failing the whole refresh request. A result with both `hint` and `error` means cached metadata is being returned after live refresh failed. Registry refresh must stream partial responses as individual packages finish and must not affect import size computation.

**NFR-012** (Critical) - The daemon must operate exclusively via static AST analysis and is prohibited from executing any code found within third-party packages. No subprocess execution, `eval`, dynamic loading, or script interpretation of package contents is permitted under any circumstance.

**NFR-013** (Critical) - The daemon must operate with read-only access limited to `node_modules` packages discovered by walking upward from the active document path. It must not use the first VS Code workspace folder as a hard read boundary, because multi-root windows and nested package workspaces can place the active document in a different dependency tree. The daemon must not write any files into the user's project tree. It may write only its own lifecycle files under VS Code global storage and cache shards under VS Code workspace storage or the configured global fallback cache base.

**NFR-014** (High) - The IPC socket or named pipe must be created with permissions that restrict access to the current user only (mode `0600` on Unix systems). On Unix targets, the daemon must remove the socket file on normal shutdown or lifecycle recycle.

**NFR-014a** (High) - Before spawning the daemon, the extension host must verify the binary's integrity by computing a SHA-256 hash of the daemon executable and comparing it against a known-good hash embedded in the extension package. If the hash does not match, the extension must refuse to spawn the daemon, log a security warning to the `Import Lens` output channel, and enter degraded mode. This prevents execution of tampered binaries.

**NFR-014b** (High) - The IPC socket path (Unix) or named pipe name (Windows) must include a component unique to the VS Code window instance (e.g., the `VSCODE_PID` environment variable or a UUID generated at extension activation) to prevent collisions when multiple VS Code windows are open in different workspaces. Each window must communicate with its own dedicated daemon instance.

### 7.5 Maintainability

**NFR-015** (High) - The Rust daemon crate must be structured so that the compression step, the OXC pipeline step, and the cache layer are each in separate Rust modules with clearly defined interfaces. Adding a new compression format must require changes in a single file only.

**NFR-016** (High) - The TypeScript extension host must be compiled to a single bundled output file using `tsdown`. It must have no runtime npm dependencies other than `@msgpack/msgpack`.

**NFR-017** (Medium) - The codebase must achieve at least 70% unit test line coverage on the Rust daemon's core computation logic. Integration tests must cover at least five real-world npm packages: lodash-es, date-fns, zod, react, and uuid. Each package must be pinned to a specific version and its full `node_modules` snapshot must be committed to the repository under `tests/fixtures/packages/<package>@<version>/`. Integration tests must resolve against these local snapshots, not against a live npm registry. This prevents test flakiness caused by upstream package updates that change tree-shaken output.

### 7.6 Extensibility

**NFR-018** (Medium) - Versioned MessagePack request/response schemas must include a `version` field (integer). Protocol v7 is the current native protocol and adds daemon-owned registry refresh and workspace report endpoints on top of v6 cache policy fields, cache status/cleanup/list/remove endpoints, v5 daemon-first document/package.json/package.json streaming partials/raw specifier/current-file size/named-export completion/node_modules change endpoints, v4 confidence metadata, v3 runtime-aware imports, and v2 streaming batch responses/export enumeration/file-level shared sizing/module breakdowns/per-frame index metadata. The daemon must reject requests with an unrecognised version number and respond with a protocol error response when the request shape allows it. Protocol v1 full-batch `BatchRequest`/`BatchResponse`, v2 request, v3 request, v4 request, v5 request, and v6 request compatibility must be preserved where the missing fields have safe defaults.

---

## 8. Acceptance Criteria

The following criteria constitute the definition of done for the v1.0 release. All five criteria must pass before a release VSIX is published to the VS Code Marketplace.

**AC-001 - Size limit compliance:** The extension installs successfully on each target platform and the installed VSIX does not exceed 20 MB for any single platform target.

**AC-002 - Latency requirement:** Typing a new import statement displays the correct size decoration within 500ms on the first attempt (cache miss). On subsequent attempts using the same import in the same session, the decoration renders in under 50ms (cache hit). Both measurements are taken on the reference machine defined in NFR-002.

**AC-003 - Missing package handling:** Deleting the `node_modules` folder while the extension is running updates all affected import decorations to display "Package not found" without throwing an uncaught error or crashing the daemon.

**AC-004 - Settings reactivity:** Changing the `importLens.compression` setting immediately updates all currently visible inline decorations to reflect the new format selection. No file change, save, or editor reload is required.

**AC-005 - Memory stability:** A memory profile of the Node.js extension host process taken before and after 5 minutes of continuous rapid typing in a JS/TS file confirms that the extension host heap does not grow continuously. The heap must return to within 10% of its pre-typing baseline between typing bursts.

**AC-006 - Loose-file support:** Opening a supported JS/TS file outside a VS Code workspace folder but inside a parent tree containing `package.json` or `node_modules` computes import sizes without showing daemon unavailable solely due to the missing workspace folder.

**AC-007 - Insight surfacing:** In a Git-tracked file with a newly added package import, the inline label or hover shows the current import-cost delta. For an `export * from "package"` statement, the hover shows a barrel re-export warning. For two imports sharing at least one reported module path, the hover or report identifies shared dependency bytes.

**AC-008 - Named export action safety:** For a namespace import whose result is not truly tree-shakeable, the lightbulb action can enumerate named exports and copy a candidate named import statement. The action must not rewrite the user's document automatically.

---

## 9. Technical Stack

### 9.0 Dependency Version Policy

Import Lens deliberately tracks recent dependency versions and stays current automatically wherever it is safe. Every dependency's version constraint — new or existing — is chosen by the **blast radius of an automatic upgrade**, tightening only as far as the real risk demands. New dependencies are added at their latest stable release resolved at implementation time. A caret or tilde range is the intended policy, not a defect to be "fixed" to an exact pin.

- **Tier 1 — track minor + patch (caret `^`):** no in-major upgrade can break the project. Applies to most well-behaved libraries and dev tooling (e.g. Biome, lefthook), and to `redb ^4`.
- **Tier 2 — patch-only (tilde `~`):** a minor bump could break, so only patches flow automatically. Applies to `papaya ~0.2`.
- **Tier 3 — exact (`=`):** reserved for the case where even a patch can break. Applies to the coordinated compiler stack — `rolldown`, the OXC monorepo crates, and `oxc_resolver`, whose pins move only through `pnpm deps:update:compiler` (see §9.3 and constraint C-001) — and to `packageManager`, because Corepack requires an exact version and integrity hash. GitHub Actions are **not** pinned to exact releases: an exact tag is still mutable, so it buys none of the protection a commit SHA would while costing an upgrade PR per release. Mutable action references are an accepted, documented risk.

**No test may assert the version of any dependency except the coordinated compiler stack.** The compiler stack (rolldown, the OXC monorepo crates, and `oxc_resolver`) is the only set of dependencies whose bump can silently change analysis output; a break anywhere else is caught by CI before it ships, and the lockfiles hold the build steady between deliberate updates. See §9.3 and `scripts/test/compiler-stack-coordination.test.mjs`.

The specific per-dependency policy for each crate and package is recorded in the manifest tables in §9.4.1–§9.4.3.

### 9.1 Extension Host (TypeScript)

| Component      | Technology                                        | Rationale                                                                                                                                                                                                                                     |
| -------------- | ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Language       | TypeScript 7.x (v7.0.2)                           | Native Go-based compiler. Uses `module: "esnext"`, `target: "es2025"`, and an explicit `types` array (`["node", "vscode"]`) in `tsconfig.json`. The TS 6 bridge release left the config already conforming, so the move to 7 required no source or tsconfig change. |
| Bundler        | tsdown (Rolldown-based)                           | Produces single-file CommonJS output (`dist/extension/extension.cjs`) with an explicit `node20` target for VS Code 1.90 extension-host compatibility, while build/test/package infrastructure runs on Node.js 24 LTS.                         |
| Editor adapter | VS Code APIs + daemon IPC                         | The extension host owns editor integration, settings, UI, hovers, commands, file watchers, and source/path IPC requests. Reusable analysis is daemon-owned so future editors can share it.                                                    |
| IPC encoding   | `@msgpack/msgpack`                                | Payloads typically 20-40% smaller than JSON; meaningful improvement for batch responses of 20+ imports                                                                                                                                        |
| IPC transport  | Unix socket (macOS/Linux) or Named pipe (Windows) | Multiplexed, no stdout pollution                                                                                                                                                                                                              |
| File watching  | `vscode.workspace.createFileSystemWatcher`        | Native VS Code API; manages inotify/FSEvents limits safely across all extensions; used to detect package.json changes in node_modules and tsconfig/jsconfig changes in the workspace, and to trigger daemon cache and resolver invalidation   |
| Registry queue | Daemon-owned queue (v7+)                          | Daemon npm registry refresh uses bounded concurrency, interval rate limits, in-flight de-duplication, timeout, retry, and `Retry-After` handling without an extension-host queue dependency.                                                  |
| Telemetry      | `vscode.env.createTelemetryLogger` (v1.1 target)  | Anonymised usage telemetry (cache hit rate, tier distribution, recycle frequency). Opt-out respects VS Code global telemetry setting. Instrumentation scaffolding may be added in v1.0 with reporting deferred to v1.1.                       |

### 9.2 Rust Daemon

| Component                  | Crate                        | Rationale                                                                                                                                                                                                                              |
| -------------------------- | ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Root module resolution     | `oxc_resolver` (v11.x)       | Production-ready, 30x faster than webpack's enhanced-resolve, used by Rolldown and Nuxt. Resolves the root package request before bundling so cache identity and fast hits never construct a bundler; the engine mirrors its condition/main-field configuration for transitive resolution. Note: lives in a separate repository (`oxc-project/oxc-resolver`), versioned independently from the main OXC monorepo. |
| Parsing                    | `oxc_parser` (v0.139.x)      | ~3x faster parsing throughput than SWC on JS/TS input, arena-allocated AST, production-ready                                                                                                                                           |
| Semantic analysis          | `oxc_semantic` (v0.139.x)    | Produces scope trees, symbol tables, and binding information used to validate the linked chunk before minification.                                                                                                                    |
| Linking and tree-shaking   | `rolldown` (v1.1.x)          | Embedded Rust bundler built on OXC. Owns transitive resolution, ESM/CJS linking, binding/namespace semantics, `sideEffects` interpretation, statement/module retention, symbol deconfliction, and TS/TSX/JSX/JSON handling. Wrapped behind one narrow adapter; no Rolldown type crosses the engine contract. See FR-018 and Section 10.7. |
| `sideEffects` glob matching | `fast-glob`                  | **Rolldown's own matcher** (`rolldown_common`/`rolldown_utils` call `fast_glob::glob_match`; an OXC-project crate, already in the graph transitively through both Rolldown and `oxc_resolver`). Used only to decide whether the *measured entry* matches a declared `sideEffects` pattern — the reporting-only badge of FR-021, never retention. Promoted to a direct, exact-pinned dependency because its entire value is that it AGREES with the bundler: a second glob engine reading the same array can disagree, and then Import Lens labels a file the opposite way from how Rolldown treated it. No new supply-chain surface. Version is not chosen — `deps:update:compiler` derives it from Rolldown's own resolved graph and coordinates it in `scripts/compiler-stack.config.mjs`. |
| Minification and mangling  | `oxc_minifier` (v0.139.x)    | Dead code elimination, constant folding, branch pruning, and supported mangling metadata for codegen. Stable 0.x release line; acceptable for size estimation within 1-2% variance.                                                    |
| Code generation            | `oxc_codegen` (v0.139.x)     | Converts the minified AST back to a JavaScript string. Required because `oxc_minifier` operates on the AST, not on text. Supports `minify: true` for whitespace removal.                                                               |
| Gzip compression           | `flate2`                     | Stable, widely used, level 6 default                                                                                                                                                                                                   |
| Brotli compression         | `brotli` crate               | Level 4 balances speed and ratio for real-time use                                                                                                                                                                                     |
| Zstd compression           | `zstd` crate                 | Level 3 provides best speed-to-ratio balance                                                                                                                                                                                           |
| In-memory cache            | `papaya` (v0.2.x)            | Lock-free, deadlock-safe, optimised for read-heavy workloads. Uses a pinning API (`map.pin()`) rather than traditional lock guards.                                                                                                    |
| Persistent cache           | `redb` (v4.x)                | Stable release, pure Rust, ACID, copy-on-write B-trees                                                                                                                                                                                 |
| Concurrency                | `rayon` (v1.12.x)            | Work-stealing thread pool for parallel batch processing. Note: `rayon::join` accepts exactly 2 closures; 3+ parallel tasks require nested `rayon::join` or `rayon::scope`.                                                             |
| IPC serialization          | `rmp-serde` (v1.3.x)         | MessagePack integration with serde, no extra dependencies                                                                                                                                                                              |
| Async runtime              | `tokio`                      | Async socket server for handling concurrent IPC requests                                                                                                                                                                               |
| IPC framing                | `tokio-util` codec           | Production length-delimited framing for the existing 4-byte big-endian MessagePack payload prefix and 32 MiB maximum frame size                                                                                                        |

### 9.3 OXC Versioning Note

OXC Rust crates use 0.x versions, but that does not mean they are alpha quality. OXC follows Rust package versioning before a 1.0 line while publishing production-ready crates. Import Lens pins the OXC analysis stack to one coordinated resolved version across Rust crates so parser, semantic, minifier, and codegen APIs cannot drift independently. `daemon/Cargo.toml` must use Cargo's exact (`=`) requirement syntax (for example `=0.139.0`) for every OXC monorepo crate, for the independently versioned `oxc_resolver` crate, and for the `rolldown` crate family (`rolldown`, `rolldown_common`, `rolldown_error`), because the coordinated compiler stack moves only through its updater. Every version jump — patch included — is a coordinated, deliberate upgrade; nothing flows in through a general `cargo update`. Because even a patch bump can shift `oxc_minifier` output, the CI accuracy suite (`pnpm test:accuracy`, run on every push and pull request) is the safety net that catches drift the committed `Cargo.lock` lets through — the lock only moves on an intentional `cargo update`. That suite detects only the drift its fixtures can express, so the fixtures must reach the paths an OXC release can move: real npm packages pinned by a committed lockfile (`css-tree`, `date-fns`, `lodash` — that one because it declares no `module` field and is therefore the only fixture that drives the engine's CJS link-time interop path — and `refractor`, the only fixture whose `sideEffects` carries a **package-root-anchored** glob, i.e. a pattern containing a `/`), plus a TypeScript package, the only fixture that drives the engine's TypeScript transform path. The `refractor` precondition is guarded, not assumed: a pattern with no separator is matched with a `**/` prefix and so matches an *absolute* path by accident, which is how a wrong package-relative path stayed green across every offline test while `refractor` was reported at 30,229 B against a real 114,296 B (esbuild). Only an anchored pattern goes red, and this suite — real bytes against an independent bundler — is the only one that answers for a wrong number on a real package. Real fixtures are downloaded on demand; a failed download degrades to a warned skip locally and must be a hard failure under `IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES=1`, which CI sets. Coordinated minor/major OXC upgrades must be performed as an intentional batch with lockfile updates and the compiler-stack coordination test suite, capturing the accuracy byte counts before and after and tracing every difference to a specific upstream change; an unexplained difference blocks the upgrade. The repository must provide `pnpm deps:update:compiler` for targeted stack upgrades, supporting explicit versions, Cargo-derived latest resolution, and dry-run mode while updating `daemon/Cargo.toml`, `scripts/compiler-stack.config.mjs`, the generated `scripts/compiler-stack.fingerprint.json`, lockfiles, and this SRS together. Tests must never carry a stack version literal: `scripts/test/compiler-stack-coordination.test.mjs` derives its expectations from `scripts/compiler-stack.config.mjs`, which is the single source of truth for the resolved versions. The updater must fail before edits when requested versions are invalid, unavailable, or unsatisfiable as one Cargo-resolved graph, OXC monorepo crate versions are not coordinated, exact pins are missing, or `oxc_mangler` is reintroduced. `oxc_resolver` is versioned independently in a separate repository and is pinned separately. The Docker builder plus `rust-toolchain.toml` follow stable Rust so dependency MSRV bumps are picked up during deliberate upgrade runs. The Docker cross-build toolchain also follows latest stable Zig and latest `cargo-zigbuild` by default, with exact build-arg overrides available only for emergency bisects. Minifier output can differ from SWC by 1 to 2 percent; that variance is acceptable for inline size estimates. See constraint C-001 in Section 13.1.

### 9.4 Dependency Manifest (Current Resolved Versions)

> **This table tracks the current resolved dependency versions and the intended upgrade policy.** The coordinated compiler stack (rolldown, the OXC monorepo crates, and `oxc_resolver`) is exact-pinned (`=`) and moves only through `pnpm deps:update:compiler`, which lets Cargo derive a compatible stack and regenerates the graph fingerprint; `Cargo.lock` and `pnpm-lock.yaml` provide reproducible builds between upgrade runs. Use `pnpm deps:update:safe` for a broad refresh of everything else — it advances each dependency to the newest version that still satisfies its declared range (`pnpm update` within the `package.json` caret/tilde/exact ranges, `cargo update` within `Cargo.toml`'s), then restores the recorded compiler stack and fails if the resolved graph no longer matches the committed fingerprint. Re-run the compiler-stack coordination and `pnpm test:accuracy` suites after either path. OXC versioning policy last audited: **10 July 2026.**

#### 9.4.1 Rust Crates (`Cargo.toml`)

| Crate             | Current Resolved Version | Version Policy | Stability       | Notes                                                                                                                                                                                      |
| ----------------- | ------------------------ | -------------- | --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `oxc_parser`      | 0.139.0                  | `=` exact pin  | ✅ Stable API    | OXC monorepo crate. Must be upgraded in lockstep with the other OXC monorepo crates.                                                                                                       |
| `oxc_resolver`    | 11.23.0                  | `=` exact pin  | ✅ Stable        | Separate repo from OXC monorepo; versioned independently and upgraded separately.                                                                                                          |
| `rolldown`        | 1.1.5                    | `=` exact pin  | ⚠️ No Rust-API semver | The production semantic bundler since the Phase 3 cutover (with its `rolldown_common`/`rolldown_error` siblings at the same monorepo version). Every size-producing path links and tree-shakes through it. Coordinated with the OXC stack through `pnpm deps:update:compiler` and the generated graph fingerprint; every version bump re-runs the bundler-redesign qualification gates. |
| `oxc_semantic`    | 0.139.0                  | `=` exact pin  | ✅ Stable API    | Must match `oxc_parser` resolved version.                                                                                                                                                  |
| `oxc_minifier`    | 0.139.0                  | `=` exact pin  | ✅ Stable API    | Test every upgrade against the accuracy suite because minified output can shift across releases. The daemon uses the minifier result's scoping and private-member mappings for codegen.    |
| `oxc_codegen`     | 0.139.0                  | `=` exact pin  | ✅ Stable API    | Required for AST -> string. Use `minify: true`.                                                                                                                                            |
| `oxc_allocator`   | 0.139.0                  | `=` exact pin  | ✅ Stable        | Arena allocator. Must match parser resolved version.                                                                                                                                      |
| `oxc_span`        | 0.139.0                  | `=` exact pin  | ✅ Stable        | Source locations. Must match parser resolved version.                                                                                                                                      |
| `oxc_syntax`      | 0.139.0                  | `=` exact pin  | ✅ Stable API    | Syntax metadata used by the parser and downstream OXC stages. Must match parser resolved version.                                                                                          |
| `papaya`          | 0.2.4                    | `~0.2`         | Pre-1.0         | Uses pinning API (`map.pin()`). Recycle triggers at 200,000 cached entries (NFR-004a). Watch for breaking changes.                                                                         |
| `redb`            | 4.1.0                    | `^4`           | ✅ Stable (1.0+) | ACID, committed file format. Upgraded from v3 to v4 (April 2026). The redb file format is committed stable; the v3→v4 migration must be handled via cache schema versioning (see FR-026a). |
| `rayon`           | 1.12.0                   | `^1.12`        | ✅ Stable        | `join()` takes exactly 2 closures. Use nested calls for 3+.                                                                                                                                |
| `rmp-serde`       | 1.3.1                    | `^1.3`         | ✅ Stable        | MessagePack ↔ serde.                                                                                                                                                                       |
| `serde`           | 1.0.228                  | `^1`           | ✅ Stable        | With `derive` feature.                                                                                                                                                                     |
| `serde_json`      | 1.0.x                    | `^1`           | ✅ Stable        | Structured parsing for `package.json` metadata and small lifecycle files such as `importlens-recycles.json`; avoids ad hoc string parsing.                                                 |
| `tokio`           | 1.52.3                   | `^1.52`        | ✅ Stable        | Features: `rt-multi-thread`, `net`, `io-util`, `macros`.                                                                                                                                   |
| `tokio-util`      | 0.7.18                   | `^0.7`         | ✅ Stable        | Length-delimited codec for Rust IPC frames, configured for the existing 4-byte big-endian length prefix and 32 MiB max frame size.                                                         |
| `bytes`           | 1.11.1                   | `^1`           | ✅ Stable        | Frame payload buffer type used by `tokio-util` codec sinks.                                                                                                                                |
| `futures-util`    | 0.3.32                   | `^0.3`         | ✅ Stable        | `SinkExt`/`StreamExt` utilities for framed IPC read/write loops.                                                                                                                           |
| `flate2`          | 1.1.9                    | `^1.1`         | ✅ Stable        | Gzip level 6.                                                                                                                                                                              |
| `brotli`          | 8.0.3                    | `^8`           | ✅ Stable        | Brotli level 4.                                                                                                                                                                            |
| `zstd`            | 0.13.3                   | `~0.13`        | ✅ Stable API    | Zstd level 3.                                                                                                                                                                              |
| ~~`num_cpus`~~    | N/A                      | N/A            | Removed         | Replaced by `std::thread::available_parallelism()` (stable since Rust 1.59). The `num_cpus` crate is in maintenance mode and provides no value over the stdlib.                            |

#### 9.4.2 npm Packages (`package.json`)

| Package            | Current Resolved Version | Category        | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ------------------ | ------------------------ | --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `@msgpack/msgpack` | 3.1.3                    | `dependency`    | MessagePack encode/decode.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `tsdown`           | 0.22.4                   | `devDependency` | Rolldown-based bundler. Output: single-file `dist/extension/extension.cjs` CommonJS bundle targeting Node 20 syntax for VS Code 1.90 extension-host compatibility.                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `typescript`       | 7.0.2                    | `devDependency` | Native Go-based compiler. Type checking only; not a runtime dep. **tsconfig must use**: `module: \"esnext\"`, `target: \"es2025\"`, `types: [\"node\", \"vscode\"]` (explicit), `moduleResolution: \"bundler\"`. Do NOT fall back to TS 5.x or 6.x.                                                                                                                                                                                                                                                                                                                                                                      |
| `@types/vscode`    | `^1.90.0`                | `devDependency` | Tracks the baseline VS Code version, not the latest release. The extension's `package.json` must declare `"engines": { "vscode": "^1.90.0" }`. All VS Code APIs used by Import Lens (InlayHintsProvider, FileSystemWatcher, OutputChannel, TelemetryLogger, etc.) are available in 1.90+. VS Code 1.90 was released in May 2024; this baseline keeps compatibility with the popular VS Code forks (Cursor, Windsurf, Antigravity) that lag upstream. The caret range and `pnpm-lock.yaml` hold it at 1.90.0 today. **Accepted risk:** a deliberate `pnpm update` may float the types above the `engines.vscode` floor, letting `tsc` compile calls to APIs absent from the minimum supported VS Code — a failure that reaches users rather than CI. No test guards this. |
| `@types/node`      | 24.13.3                  | `devDependency` | Explicit Node ambient types for Node APIs used by Import Lens (`fs/promises`, `net`, `child_process`, `crypto`, `path`, and Node's built-in test runner). Build infrastructure runs on Node 24 LTS, but this ambient type baseline is not raised by build-tool-only upgrades.                                                                                                                                                                                                                                                                                                                                             |
| `@vscode/vsce`     | 3.9.2                    | `devDependency` | VSIX packaging and publishing. Import Lens stages a minimal physical package tree and invokes `@vscode/vsce package --target <platform>` from that staging directory.                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `esbuild`          | 0.28.1                   | `devDependency` | Accuracy comparator baseline used by `pnpm test:accuracy`; not part of the shipped extension runtime.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |

#### 9.4.3 Build Tools

| Tool                  | Version               | Purpose                    | Notes                                                                                                                                                                                                                                                                                                         |
| --------------------- | --------------------- | -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust toolchain        | stable (Edition 2024) | Daemon compilation         | The daemon does not declare a fixed MSRV while Import Lens intentionally tracks latest package versions. `rust-toolchain.toml` and `Dockerfile.build` use stable Rust so OXC or other dependency MSRV bumps are picked up with normal toolchain updates.                                                       |
| Zig                   | latest stable         | Cross-target linker        | Used by the Docker `--zigbuild` packaging path for Linux and macOS binaries. It is not needed for the native Windows package path. `Dockerfile.build` resolves `ZIG_VERSION=latest` from Zig's official download index and still permits an exact `ZIG_VERSION` build arg for bisecting upstream regressions. |
| `cargo-zigbuild`      | latest                | Rust cross-compilation     | Wraps Cargo so Rust builds can use Zig's C toolchain/linker for Linux and macOS targets from the Linux Docker builder. The default Docker build installs the latest crate; an exact `CARGO_ZIGBUILD_VERSION` build arg may be used only to isolate a broken upstream release.                                 |
| `wasm-opt` (Binaryen) | 123                   | WASM binary optimization   | Run with `-Oz` after `cargo build --target wasm32-wasip1-threads`.                                                                                                                                                                                                                                            |
| Node.js               | 24 LTS                | Build/test/package runtime | CI, release, Docker packaging, and local build tooling run on Node.js 24 LTS. The generated extension bundle still targets Node 20 syntax because VS Code 1.90 hosts extensions on Node 20.9.0.                                                                                                               |
| pnpm                  | 11.10.0               | Package manager            | Pinned once, in `packageManager`. Corepack resolves it in CI and in the Docker builder; no workflow or Dockerfile declares a version. Requires Node.js 22.13+; Import Lens uses Node.js 24 LTS for all build infrastructure.                                                                                                                     |

#### 9.4.4 Deprecated / Banned Packages

> **These packages must NOT be used anywhere in the project.** Any appearance in `Cargo.toml`, `package.json`, or source code is a build error.

| Package                  | Reason                                                          | Replacement                                                     |
| ------------------------ | --------------------------------------------------------------- | --------------------------------------------------------------- |
| `@oxc-parser/wasm` (npm) | Officially deprecated. No longer maintained.                    | Rust `oxc_parser` in the daemon                                 |
| `oxc-parser` (npm)       | Node-side reusable import parsing would duplicate daemon logic. | Rust `oxc_parser` in the daemon                                 |
| `sled` (Rust)            | Never shipped 1.0. Unstable on-disk format.                     | `redb` (v4.x, stable format)                                    |
| `dashmap` (Rust)         | Deadlock risk with sharded RwLock under read-heavy workloads.   | `papaya` (v0.2.x, lock-free)                                    |
| `num_cpus` (Rust)        | Maintenance mode since June 2023. Superseded by stdlib.         | `std::thread::available_parallelism()` (stable since Rust 1.59) |

---

## 10. Component Specifications

### 10.1 IPC Message Schemas

#### BatchRequest

```typescript
interface BatchRequest {
  version: number;              // Protocol version, currently 7
  request_id: number;           // Monotonic counter incremented per debounce cycle.
                                // The daemon echoes this value in BatchResponse.
                                // The extension host discards responses whose
                                // request_id does not match the most recently sent value.
  workspace_root: string;       // Absolute path to the active analysis root.
  active_document_path: string; // Absolute path to the file currently being edited.
                                // oxc_resolver starts upward traversal from this path,
                                // not from the workspace root, to correctly resolve
                                // nested node_modules in monorepos (e.g. PNPM workspaces
                                // where packages/app-a has its own node_modules with a
                                // different version than the root-level hoisted copy).
  imports: ImportRequest[];
  streaming?: boolean;          // Protocol v2+; request indexed partial responses.
}

interface ImportRequest {
  specifier: string;         // Full import specifier including subpath, e.g. "date-fns/format"
  package: string;           // Root package name only, e.g. "date-fns" (used for node_modules lookup)
  version: string;           // Installed version read from root package.json, e.g. "3.6.0"; "unknown" for malformed/versionless manifest fallback
  named: string[];           // Named exports requested; empty for default/namespace/dynamic
  import_kind: "named" | "default" | "namespace" | "dynamic";
  runtime: "component" | "client" | "server"; // Protocol v3+; defaults to "component" when omitted by older clients
}
```

#### BatchResponse

```typescript
interface BatchResponse {
  version: number;
  request_id: number;           // Echoed from the corresponding BatchRequest.
                                // Extension host uses this to discard stale responses.
  imports: ImportResult[];
  indexes?: number[];           // Protocol v2+ streaming partials: import indexes represented
                                // by this frame. Omitted on full-batch responses.
}

interface ImportResult {
  specifier: string;
  raw_bytes: number;              // Unpacked size of the relevant module files
  minified_bytes: number;         // After OXC tree-shake and minification
  gzip_bytes: number;             // flate2 level 6
  brotli_bytes: number;           // brotli level 4
  zstd_bytes: number;             // zstd level 3
  cache_hit: boolean;
  side_effects: boolean;          // true if sideEffects is absent, true, or a glob the ENTRY matches
  truly_treeshakeable: boolean;   // false if named export size is within 5% of full package size
                                  // or the import is side-effectful (FR-021)
  is_cjs: boolean;                // true if the package has no ESM entry; size is approximate
  confidence: "high" | "medium" | "low";
  confidence_reasons: string[];   // Human-readable reasons for non-high confidence or exact zero-cost confidence
  error: string | null;           // Non-null if computation failed for this import
  diagnostics: ImportDiagnostic[]; // Structured daemon diagnostics for copy/debug flows
  module_breakdown?: ModuleContribution[]; // Top 10 module contributors by raw bytes
  shared_bytes?: number;          // Raw bytes shared with another import in the same file
}

interface ImportDiagnostic {
  stage: string;                   // Failing pipeline stage, e.g. "entry_resolution"
  message: string;                 // Exact daemon failure message for this stage
  details: string[];               // Context such as active path, package, and candidates
}

interface ModuleContribution {
  path: string;                     // Canonical module path when known
  bytes: number;                    // Raw source bytes attributed to that module
}
```

#### AnalyzePackageJsonRequest / AnalyzePackageJsonResponse

Used by package.json dependency decorations. Size analysis remains daemon-owned. Registry latest/deprecation metadata is daemon-owned. The extension host never calls the npm registry directly. The daemon maintains a centralized normalized npm package metadata cache keyed by package name. Package.json dependency analysis may request cached registry hints from the daemon without network I/O; the daemon derives each per-installed-version hint from the cached package metadata. A separate registry refresh request asks the daemon to fetch npm metadata only when the package metadata cache is missing or expired. Automatic refreshes respect freshness TTLs and cached retry windows. Manual refreshes use `force_refresh`, bypass TTL and retry-window checks, and fetch from npm unless the same package already has an active in-flight fetch to join. Refresh uses bounded concurrency, shared interval rate limiting, short timeouts, retry-after handling, hard retry limits, per-package failure isolation, per-package failure logging, and daemon-owned persistent cache storage under the extension-managed daemon cache base. The refresh request streams one partial response for each completed package so the extension can update visible package rows as soon as each registry result is available. If live refresh fails but cached metadata exists, the daemon returns both the cached hint and a per-package error; editors must keep the cached hint visible and mark it stale.

```typescript
type ImportAnalysisStatus = "loading" | "ready" | "missing" | "unavailable";
type PackageJsonDependencySectionName =
  | "dependencies"
  | "devDependencies"
  | "peerDependencies"
  | "optionalDependencies";

interface AnalyzePackageJsonRequest {
  type: "analyze_package_json";
  version: number;              // Protocol version, currently 7
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  source: string;
  streaming?: boolean;          // Protocol v5+; request indexed package.json partial responses.
  include_registry_hints?: boolean; // Deprecated in v7+; daemon-owned registry refresh via RefreshRegistryHintsRequest.
  force_registry_refresh?: boolean; // Deprecated in v7+; use RefreshRegistryHintsRequest with mode: "force_refresh".
  refresh_section?: "dependencies" | "devDependencies" | "peerDependencies" | "optionalDependencies";
}

interface AnalyzePackageJsonResponse {
  version: number;
  request_id: number;
  sections: PackageJsonDependencySection[];
  states: PackageJsonDependencyAnalysisItem[];
  indexes?: number[];           // Present only on streaming partials; indexes into final dependency state order.
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

interface PackageJsonDependencyAnalysisItem {
  entry: PackageJsonDependencyEntry;
  name: string;
  section: PackageJsonDependencySectionName;
  status: ImportAnalysisStatus;
  installedVersion?: string;
  registryHint?: RegistryHint | null;
  message?: string;
  result?: ImportResult;
}

interface PackageJsonDependencyEntry {
  name: string;
  version: string;
  section: PackageJsonDependencySectionName;
  range: SourceRange;
  nameRange: SourceRange;
  valueRange: SourceRange;
}

interface PackageJsonDependencySection {
  section: PackageJsonDependencySectionName;
  range: SourceRange;
  objectRange: SourceRange;
}

interface RegistryHint {
  latestVersion?: string;
  latestPublishedAt?: string;
  isLatest?: boolean;
  deprecated?: boolean;
  fetchedAt?: number;
}
```

#### HelloMessage

Sent by the extension host immediately after opening the socket connection. The daemon must validate the hello protocol version before accepting the handshake and must not process any request until a valid `HelloMessage` has been received.

```typescript
interface HelloMessage {
  type: "hello";
  version: number;              // Protocol version, currently 7
  workspace_root: string;       // Absolute path to the active analysis root.
  storage_path: string;         // Absolute extension-owned cache base; daemon creates project shards below it
  enable_disk_cache: boolean;   // From importLens.enableDiskCache setting
  cache_max_size_mb: number;    // From importLens.cacheMaxSizeMB
  cache_max_age_days: number;   // Deprecated and ignored; kept for wire compatibility
  log_level: "error" | "warn" | "info" | "debug";
}
```

After accepting a valid `HelloMessage`, the daemon starts best-effort recent-cache prewarm work for the active workspace. Protocol-bearing requests sent before hello receive protocol errors or are ignored as specified in Section 6. Unsupported hello versions are logged by the daemon and cause the connection to close without processing later frames from that socket.

#### CacheInvalidateMessage

Sent by the extension host when the file watcher detects a change in `node_modules`. The daemon must evict all matching cache entries from both `papaya` and `redb`.

```typescript
interface CacheInvalidateMessage {
  type: "cache_invalidate";
  package: string;              // Package name (including scope prefix for scoped packages, e.g. "@babel/core")
}
```

#### CacheInvalidateAllMessage

Sent by the extension host when the entire `node_modules` tree is deleted or replaced (e.g. after `rm -rf node_modules` or a fresh `npm install` that changes multiple package versions simultaneously). The daemon must evict all entries from both `papaya` and `redb`. The extension host must send this message instead of individual `CacheInvalidateMessage` calls when more than 20 packages are invalidated in a single file-watcher event burst, to avoid saturating the IPC socket with hundreds of small messages.

```typescript
interface CacheInvalidateAllMessage {
  type: "cache_invalidate_all";
}
```

#### NodeModulesChangedMessage

Sent by the extension host for a debounced watcher burst. It carries the two kinds of file the daemon memoizes and cannot see change on its own.

`package_json_paths` holds 1 through 20 concrete `node_modules/**/package.json` paths. The daemon must derive package names from the paths and evict those packages. If the `package.json` path set is larger than 20, or every path in it fails to map to a package name, the daemon must treat that half as `CacheInvalidateAll`.

`tsconfig_paths` holds the workspace's changed alias-table configs (FR-027a). The daemon must drop the shared resolvers and the L1 aggregate cache, and nothing else. The field is optional on the wire (`#[serde(default)]`), so an older client that sends only `package_json_paths` still decodes. A message may carry both halves, and both must be applied — an install and a config edit can land in the same burst.

```typescript
interface NodeModulesChangedMessage {
  type: "node_modules_changed";
  package_json_paths: string[];
  tsconfig_paths: string[];
}
```

#### PrewarmPackageJsonMessage

Sent by the extension host when a workspace `package.json` is opened or saved.

```typescript
interface PrewarmPackageJsonMessage {
  type: "prewarm_package_json";
  package_json_path: string;
  active_document_path: string;
}
```

#### EnumerateExportsRequest / EnumerateExportsResponse

Used by the named-import completion provider. This protocol v2+ request asks the daemon to resolve a package and return statically-known named exports from the cached or newly-built graph.

```typescript
interface EnumerateExportsRequest {
  type: "enumerate_exports";
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  specifier: string;
  package: string;
  package_version: string;
}

interface EnumerateExportsResponse {
  version: number;
  request_id: number;
  specifier: string;
  exports: string[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}
```

#### FileSizeRequest / FileSizeResponse

Used when the extension needs a file-level total that deduplicates modules shared by multiple imports in the same document. The daemon unions graph-backed ESM modules by canonical path, computes combined sizes once, and returns the original imports annotated with `shared_bytes` when possible.

Imports are grouped by runtime and built once per runtime, not once per document. A bundle request carries a single runtime and the bundler resolves the whole transitive graph under it, while Server and Client resolve dependencies under materially different conditions (`browser` alias fields, `browser` vs `node` export conditions). A document may legitimately carry both — an Astro page emits Server imports from frontmatter and Client imports from processed `<script>` blocks — so sizing every entry under one import's runtime would resolve the other's dependencies against the wrong conditions, and such a build still succeeds, producing a silently wrong total. Shared-module deduplication is preserved *within* each runtime, which is the only place it is real: Server and Client code never share a chunk in the shipped output. A package imported under two runtimes is therefore counted once per runtime, because each runtime genuinely ships its own copy.

Raw and minified totals are sums across runtime groups. Compressed totals (gzip/brotli/zstd) are computed once over the concatenated minified output of every group that built successfully, so they are a lower bound on what independent per-runtime bundles would compress to rather than a sum of them. When one runtime's build fails, only that runtime's entries degrade to conservative non-deduplicated per-import sums with diagnostics; the other groups keep their real deduplicated totals.

```typescript
interface FileSizeRequest {
  type: "file_size";
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  imports: ImportRequest[];
}

interface FileSizeResponse {
  version: number;
  request_id: number;
  raw_bytes: number;
  minified_bytes: number;
  gzip_bytes: number;
  brotli_bytes: number;
  zstd_bytes: number;
  imports: ImportResult[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}
```

#### RefreshRegistryHintsRequest / RefreshRegistryHintsResponse

Protocol v7+. Used to request daemon-owned registry metadata refresh for npm package versions. The daemon fetches the latest npm registry metadata when the cache is missing or expired, returning partial results as each package completes.

```typescript
type RegistryHintMode = "off" | "cached" | "refresh_stale" | "force_refresh";

interface RegistryHintTarget {
  name: string;
  installedVersion?: string;
}

interface RegistryHintResult {
  target: RegistryHintTarget;
  hint?: RegistryHint | null;
  error?: string | null;
}

interface RefreshRegistryHintsRequest {
  type: "refresh_registry_hints";
  version: number;
  request_id: number;
  targets: RegistryHintTarget[];
  mode: "refresh_stale" | "force_refresh";
}

interface RefreshRegistryHintsResponse {
  version: number;
  request_id: number;
  results: RegistryHintResult[];
  indexes?: number[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}
```

`RefreshRegistryHintsResponse` may be emitted multiple times for the same `request_id`. Partial responses contain `indexes` for the completed target positions and one result per completed package. The final response omits `indexes` and contains the full ordered result set. A package fetch failure sets `RegistryHintResult.error` for that package and leaves `RefreshRegistryHintsResponse.error` null unless the whole request is invalid. When stale cache fallback is available, `RegistryHintResult.hint` contains the cached metadata and `RegistryHintResult.error` contains the live refresh failure reason.

#### WorkspaceReportRequest / WorkspaceReportResponse

Protocol v7+. Used to request daemon-owned workspace report generation. The daemon scans the workspace for source files, aggregates import-related metrics, and returns the report model for the extension to render.

```typescript
interface WorkspaceReportRequest {
  type: "workspace_report";
  version: number;
  request_id: number;
  workspace_root: string;
  budgets?: {
    perImportBrotliBytes?: number;
    perFileBrotliBytes?: number;
  };
}

interface WorkspaceReportRow {
  file: string;
  imports: ImportResult[];
  totalBrotliBytes: number;
  budgetWarnings?: string[];
}

interface WorkspaceReportSummary {
  totalFiles: number;
  totalImports: number;
  totalBrotliBytes: number;
  filesOverBudget: number;
  importsOverBudget: number;
}

interface WorkspaceReportResponse {
  version: number;
  request_id: number;
  rows: WorkspaceReportRow[];
  summary: WorkspaceReportSummary;
  error: string | null;
  diagnostics: ImportDiagnostic[];
}
```

#### Cache Management Requests / Responses

Used by `Import Lens: Manage Cache`, `Import Lens: Clear Current Project Cache`, and `Import Lens: Clear All Caches`. Cache management requests are protocol v7+ and require a successful hello first.

```typescript
interface CacheShardInfo {
  shard_id: string;
  project_root: string;
  normalized_root: string;
  cache_path: string;
  size_bytes: number;
  last_used_millis: number | null;
  loaded: boolean;
}

interface CacheOperationResult {
  shard_id: string;
  project_root: string;
  cache_path: string;
  removed: boolean;
  error: string | null;
}

interface CacheStatusRequest {
  type: "cache_status";
  version: number;
  request_id: number;
  workspace_root?: string;
}

interface CacheStatusResponse {
  version: number;
  request_id: number;
  total_size_bytes: number;
  project_count: number;
  max_size_mb: number;
  max_age_days: number; // Deprecated echo; never a live limit
  last_cleanup_millis: number | null;
  current_project: CacheShardInfo | null;
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

interface CacheCleanupRequest {
  type: "cache_cleanup";
  version: number;
  request_id: number;
}

interface CacheCleanupResponse {
  version: number;
  request_id: number;
  total_size_bytes: number;
  removed: CacheOperationResult[];
  failed: CacheOperationResult[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

interface CacheListRequest {
  type: "cache_list";
  version: number;
  request_id: number;
}

interface CacheListResponse {
  version: number;
  request_id: number;
  shards: CacheShardInfo[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

interface CacheRemoveRequest {
  type: "cache_remove";
  version: number;
  request_id: number;
  scope: "current_project" | "selected" | "all";
  workspace_root?: string;
  shard_ids?: string[];
}

interface CacheRemoveResponse {
  version: number;
  request_id: number;
  removed: CacheOperationResult[];
  failed: CacheOperationResult[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}
```

#### ShutdownMessage

Sent by the extension host on extension deactivation. The daemon must cancel prewarm work, flush pending recency touches, and exit. See FR-038.

```typescript
interface ShutdownMessage {
  type: "shutdown";
}
```

### 10.2 Cache Key Format

The cache key for both `papaya` and `redb` size entries is a UTF-8 string using the v4 prefix and a hex-encoded MessagePack `CacheIdentity` payload:

```
v4:<hex-msgpack-cache-identity>
```

The identity payload contains `analyzer_version`, `specifier`, root `package_name`, `package_version`, optional canonical `package_root`, optional canonical `entry_path`, `runtime`, `import_kind`, and sorted/deduplicated `named_exports`. The identity is pure: file fingerprints live on the value side and are re-verified on every serve, so an edited dependency updates the same key in place instead of minting an orphan. Sorting named exports ensures import order does not create duplicate entries. Namespace, default, and dynamic imports are distinguished by `import_kind`, so a named export literally called `"dynamic"` cannot collide with dynamic-import analysis.

The `specifier` field in `ImportRequest` must carry the full subpath (e.g. `"date-fns/format"`) so the daemon can resolve the correct entry point via `oxc_resolver`. The `package` field carries the root package name only (e.g. `"date-fns"`) for `node_modules` lookup purposes. The `version` field is read from the root package's `package.json` regardless of subpath, since subpaths do not have independent versions.

Superseded by FR-026d: there is no package-directory approximation to persist. A malformed or versionless manifest yields an **Unmeasured** result, which carries no size at all. Such a result is deterministic, so it is cached like any other fact about the package (FR-026c) — but the import has no cache key derived from a resolved package, so in practice it is simply re-analyzed, which costs a manifest read and no build.

### 10.3 Virtual Entry Module

For each cache miss, the daemon constructs an in-memory virtual entry that the engine's plugin serves under a synthetic module id. Each requested package maps to a synthetic target `import-lens:target/<index>` that resolves to the pre-resolved absolute entry path from FR-017, so the bundler never re-resolves the bare package. Every requested surface gets a unique positional alias so strict entry signatures keep it alive, and names are emitted as JSON-escaped string literals so user-controlled names are never interpolated raw:

```javascript
// Named import (per requested name; string-literal names work identically)
export { "debounce" as __il_entry_0_export_0 } from "import-lens:target/0";

// Default import
export { default as __il_entry_0_default } from "import-lens:target/0";

// Namespace, dynamic, and full-package requests use the escaping-namespace
// form, because `export * from` would drop the target's default export:
import * as __il_entry_0_namespace from "import-lens:target/0";
export { __il_entry_0_namespace };
```

Dynamic-import sizing maps to the full-package form: the daemon measures the complete asynchronously loaded module cost, and code splitting is disabled so the measurement stays a single static chunk. Multi-import file sizing supplies all resolved requests as entries of one build (indexes 0..n) so shared dependencies are linked once and never double-counted.

### 10.4 Compression Pipeline

After codegen emits the minified JavaScript string, the three compression steps run in parallel using nested `rayon::join` calls:

```rust
// rayon::join accepts exactly 2 closures.
// For 3 parallel tasks, nest the second join inside the first.
let (gzip_bytes, (brotli_bytes, zstd_bytes)) = rayon::join(
    || gzip_compress(&minified_string, 6),
    || rayon::join(
        || brotli_compress(&minified_string, 4),
        || zstd_compress(&minified_string, 3),
    ),
);
```

```
minified_string (from oxc_codegen)
    ├─► flate2::GzEncoder (level 6) ────► gzip_bytes
    └─► rayon::join
        ├─► brotli::enc (level 4) ──────► brotli_bytes
        └─► zstd::encode (level 3) ─────► zstd_bytes
```

All three results are collected before the response is sent.

### 10.5 Daemon Startup and Lifecycle

```
Extension activates
    |
    ├─ Locate native binary in extension root dist/bin/<platform>/import-lens-daemon
    │       |
    │       ├─ Found → spawn process, pipe stdout/stderr, open socket, send HelloMessage handshake
    │       │
    │       └─ Not found → enter degraded mode, show status bar warning
    |
    Daemon starts
        |
        ├─ Read <globalStoragePath>/importlens-recycles.json (NFR-004b)
        │   └─ If recycle rate exceeds threshold: enter degraded mode immediately
        ├─ Remove legacy central cache at <globalStoragePath>/importlens.redb when present
        ├─ Open redb database shard at <workspaceStorage>/daemon-cache/<project-shard>/importlens.redb
        │   └─ If corrupted: delete, create fresh, log warning
        ├─ Preload at most the configured recent valid entries into papaya for the active project shard
        ├─ Serve other active-shard disk entries lazily and promote them into memory on hit
        ├─ Begin listening on socket / named pipe
        └─ After hello, prewarm up to 20 recent valid cache entries
```

### 10.6 Tree-Shakeability Detection

After computing the requested named exports, the daemon computes the full-package variant through the same bundle and minifier path. If:

```
named_export_minified_size / full_package_minified_size > 0.95
```

then `truly_treeshakeable` is set to `false`. The comparison uses minified bytes rather than raw source bytes because minified and compressed bytes are the primary user-facing size surfaces. This catches packages that declare `"sideEffects": false` in `package.json` but whose internal module graph does not actually support granular export isolation. The flag is also `false` whenever the import is **side-effectful** per FR-021 — `sideEffects` absent, `true`, or a glob **the measured entry matches** — because the comparison is only meaningful for a side-effect-free named import. A glob the entry does *not* match (`["**/*.css"]` on a JavaScript entry) is not side-effectful, so the comparison runs and the flag is measured like any other package's; it used to be gated off by construction, which is the bug FR-021 records. The full-package variant is a second engine build; if it fails, the flag degrades to `false` with a diagnostic rather than failing the analysis.

### 10.7 Bundling Engine Contract

Cross-module linking and tree-shaking are owned by the embedded Rolldown bundler. The daemon does not implement module-graph construction, reachability analysis, binding renaming, namespace materialization, or ESM/CJS interop; the previous custom module graph walk algorithm was deleted at the bundler-redesign Phase 3 cutover. `ANALYZER_REVISION` moved to `rolldown1` at that cutover and to **`rolldown2`** on 2026-07-12, when the post-cutover correctness fixes (debug-comment billing, platform/`NODE_ENV` injection, per-runtime file grouping, type-only import elision) moved real numbers.

One exception to the "does not implement" list above, recorded so it is not mistaken for dead code: the daemon **matches `package.json#sideEffects` globs against the entry it is measuring**, but that match is *reporting-only and retention-neutral*. It never reaches Rolldown and cannot change what is retained or what size is reported. It survives on the **successful measurement** path, where it decides the `side_effects` flag the UI shows beside the size, because Rolldown 1.1.5 does not expose its own retention decisions and there is no other way to tell the user whether the file they imported is one the package declared effectful. (It used to be described as surviving "solely on the static-fallback path". That path is deleted; the match is not, and it never depended on it.) See the I9 amendment in the bundler-redesign design.

The matcher itself is **not ours**. It is `fast_glob::glob_match` — the crate `rolldown_common` and `rolldown_utils` match `sideEffects` with — called with Rolldown's own pattern normalization mirrored around it, and exact-pinned into the coordinated compiler stack at the version Rolldown resolves. It used to be ~80 hand-rolled lines (brace expansion, path-component matching, segment matching), which was harmless only while the array form's answer was thrown away: two glob engines reading one `sideEffects` array can disagree, and the moment that answer decided a user-facing badge, a lookalike matcher became a way to contradict the bundler whose retention we are describing. [ADR-0002]: where upstream vendors a component, we use *that* component.

This section specifies the engine boundary (`daemon/src/engine/`) that isolates Rolldown behind Import Lens-owned types. The authoritative design, qualification record, and construct matrix live in `docs/superpowers/specs/2026-07-10-bundler-redesign-design.md` and `daemon/tests/candidate_matrix.rs`.

**Contract.** Only the engine adapter and its native plugin may import Rolldown types; no public or persistent type contains one. Callers submit a `BundleRequest` (entries with pre-resolved `entry_path`, package root, and selection — named/default/namespace/full — plus the runtime profile and purpose) and receive either a `BundleArtifact` or a typed `BundleFailure`. **No `sideEffects` metadata crosses this boundary**, and that is the contract rather than an omission: Rolldown reads the package's declaration itself, from the manifest the plugin supplies alongside the entry, and FR-021 makes it the only authority on retention. The daemon's own reading of the field decides a badge, not a byte, so it stays on the pipeline's side. (An entry used to carry a `reported_side_effects` mode; its one and only reader was the conservative diagnostic FR-021 has since retracted.) Artifact invariants:

- `code` is one complete, parseable, unminified ESM chunk.
- `loaded_paths` contains every internal real file loaded during the scan — including modules later removed by tree-shaking — canonicalized, sorted, and deduplicated.
- `contributions` contains only modules rendered into the output, using Rolldown's rendered module lengths; they are pre-minification approximations and are not required to sum to the final chunk length.
- `exported_names` comes from the entry chunk's public export list, never from a custom export walker.
- Diagnostics are plain strings with stage labels; they contain no Rolldown types or debug representations. **A warning carries the stage it came from, exactly as an error does** — an unresolved import is a `resolve` diagnostic whether the bundler externalized it (a warning, on a build that SUCCEEDS) or failed on it. Warnings were once stamped `generate` wholesale, which mislabelled the disclosure a user is most likely to meet: the one saying that bytes this package imports are not installed and are therefore not in the number beside it.
- A failure never returns partially linked code for measurement, and a missing or ambiguous requested export is a typed `missing_export`/`ambiguous_export` failure with zero-size semantics — never a guessed binding.

**Plugin responsibilities.** The native plugin does exactly three things: resolve and load the virtual entry (Section 10.3), map each synthetic target to its pre-resolved real entry path **together with that package's root `package.json` path**, and record resolved/loaded real paths while enforcing the hard limits (2,000 internal modules, 20 MiB per module source, 100 MiB total module source; a breach is a typed `module_graph_limit` failure, never a partial graph). All other resolution delegates to the plugin context resolver with self-skipping. The plugin never inspects ASTs, classifies `sideEffects`, matches globs, binds imports/exports, decides statement liveness, implements interop, renames symbols, or rewrites real module source.

**Supplying the manifest is what makes FR-021 true of the entry module.** Rolldown builds a plugin-resolved id's package metadata from the resolve hook's `package_json_path` field and from nothing else. Pre-resolving the entry (FR-017) therefore *withholds* that metadata unless the plugin hands it back: the entry module — and only the entry module, since every transitive module is resolved by Rolldown itself — would have no `package.json` at all. Two things then silently fall back to guesswork, on the one file every measurement is rooted at:

- its `sideEffects` declaration is invisible, so retention inside the entry is decided by source analysis rather than by the package's declaration — which is precisely the override FR-021 forbids, arrived at by omission instead of by a hook; and
- its `"type"` is invisible, so the entry's module format is `Unknown` and Rolldown emits the CommonJS-importer interop for its static CommonJS imports rather than Node's ESM interop.

Supplying the path is **metadata supply, not a semantic override**: the plugin hands Rolldown the **package-root** manifest — `<package_root>/package.json`, the one file whose location the pre-resolution already established — and Rolldown alone decides what it means. The hook's `side_effects` field stays unset. When a `BundleEntry`'s package root holds no manifest, no path is supplied — there is nothing for Rolldown to read, and pointing it at a file that does not exist fails the whole build (it *reads* the path).

**What the package-root manifest does and does not cover.** Rolldown makes two different lookups against a module's `package.json`, and `package_json_path` is one field:

- `sideEffects` is read from the manifest the resolver finds by walking **up to the package root**: for a path inside `node_modules` — which is every package a user imports, and therefore every entry this supply exists for — it takes the topmost manifest before the `node_modules` boundary. (Outside `node_modules` the same resolver function takes the *nearest* manifest instead; that branch is reached only by engine qualification fixtures, where supplying the package root is the more correct choice anyway, since that is where `sideEffects` is declared.) The supplied path is exactly that manifest, so the entry's `sideEffects` declaration is the package's own — which is what makes FR-021 true of the entry module, and it is the reason this metadata is supplied at all.
- `"type"` (the module format) is read from the **nearest** manifest above the file. That coincides with the package root **only when no manifest intervenes between the root and the entry file**. In the dual-package layout (`{"main":"./esm/index.js"}` at the root, `{"type":"module"}` in `esm/`) it does not, so the pre-resolved entry's format is decided from the root manifest and its static CommonJS imports get the CommonJS-importer interop instead of Node's ESM interop.

The second bullet is a real gap, not a claim this document will pretend away: one field cannot answer two lookups. It is unchanged from before any manifest was supplied, so it is not a regression, and it is recorded as **C6** in [known-issues.md](known-issues.md). Closing it needs an upstream API that takes the nearest manifest separately.

**Fixed build options.** ESM output format; strict entry signatures; source maps disabled; code splitting disabled so dynamic imports inline into the single chunk; minification disabled; one virtual entry; resolve condition names and main fields mirrored from the direct resolver configuration per runtime; Node builtins and unresolved externals stay external with structured diagnostics. The build must produce exactly one JavaScript **chunk**; more than one is a typed `output_shape` failure.

**Uncounted assets are recorded by the PLUGIN, not emitted by Rolldown.** Rolldown 1.1.5 does not emit a CSS asset — it refuses the build outright at the **link** stage (`UNSUPPORTED_FEATURE: Bundling CSS is no longer supported`). So the plugin intercepts a non-JS module at `load`, records its byte count, and returns an empty module, which is what lets the JavaScript graph be measured at all. `uncounted_assets` is that record (FR-018a). Any claim that an emitted asset is disclosed and the chunk measured beside it describes a mechanism this bundler does not have.

**Dependency fingerprints.** Freshness uses every real path the engine loaded (plus package manifests), not only rendered modules, because editing a tree-shaken module can change export resolution or future retention. A build that FAILED loaded no paths to fingerprint, so an **Unmeasured** result is fingerprinted against the package manifest and the resolved entry instead — which is what expires it when the package changes. (This paragraph described a "static/oversized fallback result" long after `analyze_static_entry` and `estimate_minified_source` were deleted. There is no fallback result and no fallback size: a failure has no size at all, and the fingerprints exist so that a *cached deterministic failure* is re-tried exactly when the bytes it is a fact about change.)

**Execution boundary.** Engine builds run as async work behind the daemon-wide two-permit boundary described in FR-023; cache hits bypass it and never construct a bundler. Blocking cache, fingerprint, minifier, and compression work stays off the async I/O threads.

**Failure policy.** **There is no static fallback.** Failures are typed by stage and produce an **Unmeasured** result under that stage (FR-026d) — no size, never a fabricated symbol, never a measurement of partially linked output, and never the entry file or the package directory measured in the bundle's place. `analyze_static_entry`, `approximate_directory_size` and `estimate_minified_source` are **deleted**; this table described them in the present tense long after they were gone, which is how the same defect kept being reintroduced.

**One build, several failures: the EARLIEST stage wins.** A build can fail in more than one module at more than one stage, and it must report the same stage every time it is run on the same bytes — the stage is the whole of what a user is told (FR-026d leaves no size to soften it) and a deterministic stage is *cached* (FR-026c), so a stage decided by chance is a durable wrong answer. Rolldown accumulates its diagnostics from module tasks it runs **concurrently**, so their order is a race: a package with a parse error in one module and an unresolved import in another reported `parse` on 38 runs and `resolve` on 10 of the same 48. The reported stage is therefore the **earliest present in pipeline order** — `resolve` → `module_graph_limit` → `parse` → `missing_export` → `ambiguous_export` → `link` → `generate` → `output_shape` — because the earliest failure is the likeliest root cause and the later ones are frequently its shrapnel, and because a new stage is then ranked by where the build reaches it rather than by someone's judgement of severity. The order is the declaration order of the stage vocabulary itself (`engine::stage`), so a stage cannot exist without a rank. The failure's message and its diagnostic list are ordered the same way, for the same reason: they are cached too.

The three **transient** stages rank ahead of every stage above, and cannot lose a comparison to one. Today they cannot even enter one — `panic`, `timeout` and `engine_gone` are constructed at the execution boundary, where the build's diagnostics do not exist (a panic unwinds past the classifier; a timeout drops the future), so such a failure carries none. The ordering exists so that if that ever changes, the answer that is never cached is the one that wins by default (FR-026c).

| failure stage | behavior |
| --- | --- |
| root package cannot be resolved | **Unmeasured** under `package_resolution` / `package_manifest` / `entry_resolution` (FR-026d). Two shapes are NOT failures and must not be treated as gaps (FR-024a): a **declarations-only** package, which resolves to no runtime entry *because it ships no runtime code* and is answered **Measured** at zero, High confidence, with a `types_only` diagnostic; and a **path alias**, whose specifier is not a package at all — it resolves through tsconfig `paths` to first-party source, contributes no third-party bytes, and is reported with a `path_alias` diagnostic. Both distinctions are load-bearing: treating either as an unresolved entry makes every file importing `@types/…`, or every file using `@app/…` aliases, a permanent floor. A specifier that resolves to **nothing** is neither, and stays a floor. |
| engine cannot resolve an internal import | legitimate external boundary preserved when possible, with an `external` diagnostic and Medium confidence; otherwise **Unmeasured** under `resolve` |
| `missing_export` / `ambiguous_export` | **Unmeasured** — the daemon must not guess a binding |
| parse / link / generate failure | **Unmeasured** under that stage. Deterministic: a property of the package's bytes, so it IS cached (FR-026c) — and it still makes its file's total a floor (FR-024a). Both are true. |
| `output_shape`, `module_graph_limit`, `oversized_entry` | **Unmeasured** under that stage. Deterministic, and cached. |
| `panic` (an engine build unwound into the boundary's `catch_unwind`) | **Unmeasured** under `panic`. The panic is contained to that one import, and every other import of the request — including every one already answered from cache — is unaffected. **Transient**: written to no cache and judged by no budget (FR-026c). |
| `timeout` (an engine build did not finish within `BUILD_TIMEOUT` and was cancelled) | **Unmeasured** under `timeout`. It carries exactly one meaning: a build that ran out of its own 8s clock. This is the containment for a build parked by a panic swallowed inside a bundler-spawned module task — nothing unwinds, so `catch_unwind` never fires, and without cancellation the permit and the in-flight guard are held forever, wedging every later build in the daemon's lifetime. The timeout bounds a **build**, so a permit is never held forever. It does **not** bound a request. For the interactive path that costs nothing, because a document analysis no longer waits for the builds its imports miss on (FR-004a) — a parked build delays one import's number and nothing else. `AnalyzeSpecifiers` still waits, deliberately (FR-004b), so a parked build there does add its full 8s to that one command, which is the price of a comparison that is complete or honestly failed. **Transient**: never written to any cache (FR-026c). A file's *combined* build is the likeliest thing in the system to hit this, being strictly larger than any single import's build — see the `degraded` rule in FR-024a. |
| `engine_gone` (the engine runtime dropped the build without replying) | **Unmeasured** under `engine_gone`. Transient. |
| OXC validation/minification failure after linking | **Unmeasured** under `minify`. Deterministic (a property of the linked bytes), and cached. |
| compression failure | a computation error with no size. **Not** durable: `flate2`/`brotli`/`zstd` fail on a valid string only through allocation failure or IO — a condition of the machine, not of the package. |

---

## 11. Data Models

### 11.1 Persistent Cache Schema (redb)

Each project cache shard is a directory under the extension-owned cache base. The shard directory name is a stable `v1-<hash>` identifier derived from the normalized analysis root. Each shard contains one `redb` database and a small JSON metadata file recording `shard_id`, `project_root`, `normalized_root`, and `last_used_millis`. The metadata file powers cache management UI without opening every database. Loaded shards must update `last_used_millis` in memory on each access, but JSON metadata writes may be throttled to avoid repeated filesystem writes during parallel import batches.

The `redb` database schema version is `6` and contains these tables:

| Table name   | Key type                                      | Value type                                                                    |
| ------------ | --------------------------------------------- | ----------------------------------------------------------------------------- |
| `metadata`   | `&str`                                        | `u64` (`schema_version`)                                                      |
| `size_cache` | `&str` (cache key as defined in Section 10.2) | `&[u8]` (8-byte little-endian `last_seq` prefix + MessagePack cache envelope) |

`size_cache` values persist an internal cache envelope containing the public `ImportResult`, analyzer version, package identity, dependency fingerprints, and full contribution list needed for accurate shared-byte accounting. The daemon normalizes `cache_hit` to `false` before writing and sets it to `true` when serving a memory or disk hit. The fixed sequence prefix lets recency scans (startup preload, byte-budget eviction, per-shard rollups) read each entry's recency without deserializing the envelope. Capacity is enforced by a global byte budget with entry-granular least-recently-used eviction across shards (per-project floor of the newest 128 entries), plus threshold-triggered database compaction so freed pages return to the filesystem.

### 11.2 Extension Global Storage

The extension stores lightweight UI history in VS Code `globalState`. These records are separate from the daemon's `redb` cache and are not used for daemon cache identity or correctness.

| Key                              | Value shape                 | Purpose                                                                                  |
| -------------------------------- | --------------------------- | ---------------------------------------------------------------------------------------- |
| `importLens.bundleImpactHistory` | `BundleImpactHistoryItem[]` | Recent current-file total measurements shown by `Import Lens: Show Bundle Impact History` |
| `importLens.importCostHistory`   | `ImportCostHistoryItem[]`   | Recent per-import measurements used to show trend notes in import hovers                 |

`BundleImpactHistoryItem` stores timestamp, file path, raw/minified/gzip/brotli/zstd byte totals, and import count. `ImportCostHistoryItem` stores timestamp, specifier, import kind, sorted named exports, raw/minified/gzip/brotli/zstd byte values, and a stable identity composed from specifier, import kind, runtime, and named export list. Both histories are bounded and newest-first. Repeated per-import entries with unchanged byte values should not create duplicate consecutive history records.

### 11.3 Configuration Storage

User configuration is stored by VS Code in the user's `settings.json` and accessed via `workspace.getConfiguration('importLens')`. The daemon does not read VS Code settings directly; the extension host passes relevant configuration values and the VS Code `globalStoragePath` in the `HelloMessage` handshake at startup.

---

## 12. Distribution and Packaging

### 12.1 Platform-Specific VSIX Strategy

The extension is published as separate platform-specific VSIX packages. VS Code automatically selects and installs the package matching the user's platform.

| VSIX target    | Daemon binary               |
| -------------- | --------------------------- |
| `linux-x64`    | `x86_64-unknown-linux-gnu`  |
| `linux-arm64`  | `aarch64-unknown-linux-gnu` |
| `darwin-x64`   | `x86_64-apple-darwin`       |
| `darwin-arm64` | `aarch64-apple-darwin`      |
| `win32-x64`    | `x86_64-pc-windows-msvc`    |
| `win32-arm64`  | `aarch64-pc-windows-msvc`   |

> **Note:** `linux-armhf` (`armv7-unknown-linux-gnueabihf`) and the WASM fallback target are deferred to v1.1. ARMv7 is increasingly uncommon for developer workstations. Adding it later requires only a new CI cross-compilation target and VSIX entry. Adding WASM later requires a proven worker runtime and packaging path.

### 12.2 Estimated Size per User Download

| Component                                        | Uncompressed               | In VSIX (compressed)    |
| ------------------------------------------------ | -------------------------- | ----------------------- |
| Native Rust daemon (OXC pipeline, stripped, LTO) | ~12-15 MB                  | ~9-11 MB                |
| `@msgpack/msgpack`                               | ~200 kB                    | ~80 kB                  |
| Extension TypeScript bundle (tsdown output)      | ~800 kB                    | ~350 kB                 |
| Metadata, icons, manifests                       | ~50 kB                     | ~20 kB                  |
| **Total per-platform VSIX**                      | **~13-16 MB uncompressed** | **~9-12 MB compressed** |

All platform targets fall within the 20 MB hard limit defined in NFR-007. Keeping reusable parsing in the Rust daemon avoids shipping a second native parser binding in the VSIX.

### 12.3 Cargo.toml Release Profile

```toml
[profile.release]
opt-level = "z"
codegen-units = 1
lto = true
panic = "abort"
strip = true
```

### 12.4 CI/CD Pipeline Requirements

- The CI pipeline must compile the Rust daemon for all six native targets using cross-compilation.
- The CI pipeline must build each platform VSIX from a temporary staging directory whose manifest contains no `devDependencies` and only the runtime dependencies required by that target.
- When pnpm is used, each VSIX build must stage physical copies of the bundled extension, target daemon binary, CLI, manifest files, and runtime production dependencies such as `@msgpack/msgpack`, then invoke `@vscode/vsce package --target <platform>` from the staging directory. This avoids publishing pnpm junctions while keeping reusable analysis inside the daemon binary.
- The CI pipeline must measure the size of each output VSIX and fail the publish step if any target exceeds 20 MB (enforcing AC-001 and NFR-007).
- Each platform VSIX must be built and published in the same CI run to ensure version consistency across all targets.
- The integration test suite and all five acceptance criteria must pass before any VSIX is published.

---

## 13. Constraints and Assumptions

### 13.1 Technical Constraints

**C-001:** OXC Rust crates use 0.x versions, but those versions are not alpha releases. Import Lens exact-pins (`=`) `rolldown`, the OXC monorepo crates, and `oxc_resolver` as one coordinated compiler stack because parser/minifier/resolver behavior directly affects size accuracy: every version movement, patch included, is an explicit coordinated change through `pnpm deps:update:compiler` with focused parser, graph, minifier, and packaging verification. Because even a patch can shift `oxc_minifier` output, the `pnpm test:accuracy` suite runs in CI on every push and pull request to catch any drift. Size estimation accuracy of approximately plus or minus 2 percent remains acceptable for an inline hint tool. **Fallback strategy:** If `oxc_minifier` exhibits correctness regressions in the integration test suite after an upgrade, the team must pin to the last known-good version and file an upstream issue. No release VSIX will ship with a minifier version that fails the integration suite. As a last resort, the daemon may skip minification entirely and report only raw + compressed sizes, with a `(no-minify)` suffix on decorations.

**C-002:** The extension depends on a native Rust daemon for reusable analysis and therefore does not provide full analysis in browser-only VS Code environments. The deprecated `@oxc-parser/wasm` package must not be used due to its deprecated status. For VS Code for the Web, the extension enters degraded mode with no parsing or size-analysis capability.

**C-003:** Rolldown publishes an embeddable Rust crate (`rolldown` on crates.io), but its Rust API carries no semver or documentation guarantee. Every qualification gate in the bundler-redesign design (docs/superpowers/specs/2026-07-10-bundler-redesign-design.md) passed on 2026-07-11, and the Phase 3 atomic cutover made Rolldown the only semantic bundler: the custom module graph walker, reachability analysis, manual concatenation/renaming, and static CJS scanner were deleted, and `ANALYZER_REVISION` moved to `rolldown1` (and subsequently to `rolldown2` on 2026-07-12, when post-cutover correctness fixes moved reported sizes). The risk of the missing API guarantee is contained by exact pins on the `rolldown` family, the coordinated compiler-stack updater and generated graph fingerprint, one narrow adapter that no Rolldown type escapes, and mandatory requalification on every version bump. A consequence of Rolldown's caret ranges on OXC: the OXC upgrade cadence is now bounded by Rolldown releases, and the updater rejects any request that would split the stack. See Appendix C: Technology Watch.

**C-004:** A WASM daemon fallback is deferred to v1.1 or later. The candidate target is `wasm32-wasip1-threads`, which is an experimental Rust/LLVM target. Thread support requires `SharedArrayBuffer` and cross-origin isolation (`Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp`). Any future WASM binary must be compiled with an explicit `--max-memory` linker flag set to at least `67108864` (64 MB) to provide sufficient headroom for Rayon's thread stacks; larger values may be needed if bundling exceeds this during deep dependency trees. VS Code for the Web remains degraded mode in v1.0 because browser `SharedArrayBuffer` availability and local `node_modules` access are not guaranteed. The `wasi-threads` proposal used by this target is considered legacy; the industry is transitioning toward the Component Model. See Appendix C: Technology Watch.

### 13.2 Out-of-Scope Decisions

- **napi-rs native addon:** Rejected because a panic in a native addon crashes the entire VS Code extension host. See Section 4.5.
- **SWC minifier:** Rejected because its binary adds approximately 25 to 27 MB per target, violating NFR-007. See Section 4.2.
- **JSON over IPC:** Rejected in favour of MessagePack for performance reasons. See Section 4.4.
- **ESBuild:** Rejected because it is written in Go and requires managing a separate WASM execution layer from Rust. See Section 4.1.
- **`@oxc-parser/wasm`:** Deprecated npm package. Replaced by Rust `oxc_parser` in the daemon. See Section 4.3.

### 13.3 Assumptions

- Users have npm, yarn, or pnpm with hoisting installed and have run a package install command. The extension does not install packages itself.
- VS Code extension storage is writable. If workspace storage is unavailable, Import Lens falls back to its global cache base; if neither cache base is writable, the persistent cache is skipped gracefully and all results are held in memory for the duration of the session.
- Packages shipping only CommonJS are analyzed statically where possible. Literal relative `require()` graphs and common export forms produce better approximations, but dynamic or unsupported CJS still falls back conservatively. The extension will display a `CJS` indicator next to the size for these packages.
- The extension assumes `node_modules` is fully installed. It will not trigger or assist with package installation.
- The user's environment is VS Code Desktop for full functionality. VS Code for the Web provides degraded mode only.

### 13.4 Future Feature Plan

- **Dedicated cache management view:** The v1 implementation uses VS Code commands and Quick Pick flows for cache status, cleanup, current-project removal, all-cache removal, and selected project removal. A future richer view may replace or supplement this with an Import Lens-owned webview or tree view showing project name, root path, cache size, last used time, loaded state, and actions per row. The view should be more task-focused than a generic settings glob-list editor: it should make the common decisions obvious, avoid raw path editing, keep destructive actions behind confirmation, and still store cache data only in extension-owned storage.
- **Status bar icon menu:** Replace the current text-first status item behavior with an icon-led Import Lens status entry similar in density to the Copilot status item. The icon should be a simple monochrome vector asset, square `16x16` viewBox, transparent background, `currentColor`/theme-tint friendly, no gradients, shadows, bitmap embeds, or detailed text, and legible in light, dark, and high-contrast themes. If VS Code product-icon contribution is used, expose a stable icon id such as `importlens-logo` and render the status text with that icon plus short state text. Clicking the icon must open an Import Lens action menu instead of jumping directly to logs. The menu should include at least Manage Cache, Show Logs, Show Report, Compare Imports, Toggle Display Mode, and Enable/Disable Import Lens, with logs kept as one explicit option rather than the default click destination.

---

## 14. Appendix A: File Structure

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
│   │   ├── extension.ts               # activate() / deactivate(); sends Shutdown on deactivate
│   │   ├── listener.ts                # onDidChangeTextDocument, debounce
│   │   ├── languages.ts               # VS Code language selector and supported language ids
│   │   ├── workspaceContext.ts        # workspace/loose-file analysis root derivation
│   │   ├── configRefresh.ts           # visible-editor refresh on settings changes
│   │   ├── analysis/
│   │   │   ├── fileSize.ts            # current-file size summary formatting
│   │   │   ├── freshness.ts           # request freshness tracking
│   │   │   ├── gitDiff.ts             # working-tree changed-line extraction
│   │   │   ├── history.ts             # bundle and per-import history globalState helpers
│   │   │   ├── insights.ts            # extension-side analysis insight builder
│   │   │   ├── status.ts              # loading/unavailable state helpers
│   │   │   └── state.ts               # Per-document import analysis state
│   │   ├── guidance/
│   │   │   ├── packageJsonAnalysis.ts # daemon-backed package.json dependency analysis controller
│   │   │   ├── packageJsonPartial.ts  # indexed package.json partial merge helpers
│   │   │   ├── packageJsonState.ts    # package.json dependency analysis state types
│   │   │   ├── registryRefresh.ts     # daemon registry refresh orchestration and stale-hint state
│   │   │   └── substitutions.ts       # curated import substitution suggestion mapping (FR-036k)
│   │   ├── ipc/
│   │   │   ├── client.ts              # Socket/pipe connection management
│   │   │   ├── protocol.ts            # Protocol v7 IPC types
│   │   │   ├── requestIds.ts          # shared monotonic IPC request ID generator
│   │   │   └── codec.ts               # MessagePack encode/decode
│   │   ├── daemon/
│   │   │   ├── manager.ts             # daemon lifecycle and analysis transport coordination
│   │   │   ├── nativeTransport.ts     # native daemon process transport
│   │   │   ├── platform.ts            # platform target mapping
│   │   │   ├── processLifecycle.ts    # startup/shutdown process cleanup helpers
│   │   │   ├── recycleGuard.ts        # graceful recycle loop guard
│   │   │   ├── restartPolicy.ts       # crash backoff policy
│   │   │   ├── startRoot.ts           # daemon analysis root selection
│   │   │   └── knownHashes.generated.ts # generated daemon binary hashes
│   │   ├── prewarm/
│   │   │   ├── packageJson.ts         # package.json open/save prewarm registration
│   │   │   └── packageJsonHelpers.ts  # package.json path and prewarm payload helpers
│   │   ├── watcher.ts                 # vscode.workspace.createFileSystemWatcher (node_modules + tsconfig/jsconfig); sends CacheInvalidate IPC messages
│   │   ├── ui/
│   │   │   ├── currentFileSize.ts     # current-file total and bundle impact history commands
│   │   │   ├── cacheManager.ts        # cache management Quick Pick commands
│   │   │   ├── cacheManagerItems.ts   # cache management item/label builders
│   │   │   ├── cacheManagerRequests.ts # cache management IPC request builders
│   │   │   ├── decorations.ts         # End-of-line text decorations
│   │   │   ├── inlayHints.ts          # InlayHintsProvider for inlayHint display mode
│   │   │   ├── codelens.ts            # Code lens provider
│   │   │   ├── completions.ts         # Named import member completion provider
│   │   │   ├── displayGuards.ts       # display-mode enablement helpers
│   │   │   ├── format.ts              # size and display label formatting
│   │   │   ├── packageJsonDecorations.ts # package.json dependency end-of-line decorations
│   │   │   ├── packageJsonLabels.ts   # package.json dependency label formatting
│   │   │   ├── namedExportCandidatePolicy.ts # pure policy for named export CodeAction eligibility
│   │   │   ├── namedExportCandidates.ts # named export candidate QuickPick command
│   │   │   ├── statusbar.ts           # Status bar item
│   │   │   ├── tooltip.ts             # Shared MarkdownString hover content
│   │   │   ├── treeShakeActionReason.ts # pure tree-shaking action reason helper
│   │   │   ├── treeShakeActions.ts    # CodeActions for tree-shaking diagnostics and candidates
│   │   │   ├── diagnostics.ts         # Clipboard formatting for ImportResult diagnostics
│   │   │   └── report.ts              # Show Report webview
│   │   ├── logger.ts                  # OutputChannel-based diagnostic logger (FR-040)
│   │   └── config.ts                  # VS Code settings access
│   └── dist/
│       └── extension.cjs              # tsdown bundle output
│
├── daemon/                            # Rust daemon crate
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                    # Entry point, socket server, Tokio runtime
│       ├── service.rs                 # Request handlers and protocol-level response helpers
│       ├── document/
│       │   ├── imports.rs             # Rust OXC document import extraction
│       │   ├── script_regions.rs      # Svelte/Astro/Vue script region extraction and runtime labeling
│       │   ├── specifier.rs           # package/specifier filtering helpers
│       │   ├── package_json.rs        # JSON-aware dependency block/range extraction
│       │   ├── ignore.rs              # .importlensignore parsing and matching
│       │   ├── completion.rs          # named import completion context extraction
│       │   └── positions.rs           # offset-to-position mapping helpers
│       ├── ipc/
│       │   ├── mod.rs
│       │   ├── codec.rs               # MessagePack length-prefix codec
│       │   ├── server.rs              # Unix socket / named pipe listener
│       │   └── protocol.rs            # Protocol v7 serde types
│       ├── engine/
│       │   ├── mod.rs                 # Import Lens-owned request/artifact/failure types
│       │   ├── adapter.rs             # Rolldown build orchestration and output translation
│       │   ├── plugin.rs              # Native plugin: virtual entry, target mapping, limits
│       │   ├── entry.rs               # Virtual entry source generation
│       │   ├── boundary.rs            # Two-permit async execution boundary + per-build timeout
│       │   ├── scheduling.rs          # Ordered miss scheduling helpers
│       │   ├── dependency_paths.rs    # Loaded-path fingerprint sources
│       │   └── limits.rs              # Hard module/source limits
│       ├── pipeline/
│       │   ├── mod.rs
│       │   ├── resolver.rs            # oxc_resolver root resolution
│       │   ├── node_builtins.rs       # Node builtin specifier detection
│       │   ├── file_size.rs           # File-level shared import cost computation
│       │   ├── fallback.rs            # Conservative static entry sizing
│       │   ├── minify.rs              # oxc_minifier + oxc_codegen usage
│       │   └── compress.rs            # flate2 + brotli + zstd (nested rayon::join)
│       ├── cache/
│       │   ├── mod.rs
│       │   ├── key.rs                 # Cache key formatting
│       │   ├── memory.rs              # papaya HashMap (pinning API)
│       │   ├── disk.rs                # redb read/write
│       │   └── project.rs             # per-project cache shard registry and cleanup
│       ├── registry/
│       │   ├── mod.rs
│       │   ├── constants.rs           # registry TTL, timeout, retry, and concurrency constants
│       │   ├── types.rs               # normalized npm package metadata and cache entry types
│       │   ├── client.rs              # bounded ureq npm registry HTTP client
│       │   ├── cache.rs               # persistent JSON package metadata cache (atomic writes)
│       │   ├── service.rs             # refresh modes, single-flight de-dup, retry, stale fallback
│       │   └── executor.rs            # dedicated registry refresh worker pool
│       ├── report/
│       │   ├── mod.rs
│       │   ├── executor.rs            # bounded workspace report worker pool
│       │   ├── scanner.rs             # symlink-safe workspace source scanner
│       │   └── model.rs               # report rows, summary counts, duplicate groups, treemap
│       ├── lifecycle.rs                # Graceful shutdown, self-recycle (NFR-004a), recycle counter write (NFR-004b)
│       └── prefetch.rs                # Background pre-warm logic
│
├── dist/bin/                          # Native daemon binaries (gitignored, CI-populated)
│   ├── linux-x64/
│   │   └── import-lens-daemon
│   ├── linux-arm64/
│   │   └── import-lens-daemon
│   ├── darwin-x64/
│   │   └── import-lens-daemon
│   ├── darwin-arm64/
│   │   └── import-lens-daemon
│   ├── win32-x64/
│   │   └── import-lens-daemon.exe
│   └── win32-arm64/
│       └── import-lens-daemon.exe
│
└── tests/
    ├── fixtures/
    │   └── packages/                  # Pinned package.json fixtures for test stability
    └── integration/
        ├── lodash_es.test.ts
        ├── date_fns.test.ts
        ├── zod.test.ts
        ├── react.test.ts
        └── uuid.test.ts
```

---

## 15. Appendix B: Decision Log

| ID    | Decision                                                                                  | Rationale                                                                                                                                                                                                                                                                                                                                                                                                                  | Alternatives Considered                                                                                                                                                                                                                          |
| ----- | ----------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| D-001 | Separate daemon process over napi-rs native addon                                         | A panic in a native addon crashes the VS Code extension host. A separate process isolates failures completely.                                                                                                                                                                                                                                                                                                             | napi-rs native addon (rejected: crash risk to editor)                                                                                                                                                                                            |
| D-002 | OXC for the full pipeline (parse, resolve, semantic, tree-shake, minify, mangle, codegen) | Single AST representation shared across all stages eliminates re-parsing overhead. All OXC crates are embeddable in Rust. OXC is used internally by Rolldown and Vite 8. Note: OXC does not provide a standalone tree-shaker; a custom module graph walker was initially required. **Partially superseded by D-017:** linking/tree-shaking moved to Rolldown; OXC remains the document parser, root resolver, validator, and final minifier. | Rolldown Rust API (rejected at the time: no stable embedding API — since published and adopted, see D-017); ESBuild (rejected: written in Go, requires separate WASM layer from Rust)                                                            |
| D-003 | oxc_minifier over swc_core                                                                | SWC platform binaries are approximately 25 to 27 MB per target, violating the 20 MB VSIX limit. For size estimation, 1-2% accuracy variance is acceptable.                                                                                                                                                                                                                                                                 | swc_core (rejected: distribution size); Terser (rejected: requires Node.js subprocess)                                                                                                                                                           |
| D-004 | MessagePack over JSON for IPC                                                             | Payloads typically 20-40% smaller than JSON. In the Rust rmp-serde path, deserialization is consistently faster. Meaningful for batch responses of 20+ imports.                                                                                                                                                                                                                                                            | JSON (rejected: performance); Protocol Buffers (rejected: schema overhead disproportionate for this local IPC protocol)                                                                                                                          |
| D-005 | Rust `oxc_parser` in the daemon over extension-host parsing                               | Keeps reusable import/specifier/package analysis shared by VS Code, CLI, and future editors. Returns ESM import info directly from OXC module records without an extension-host AST walk or runtime parser dependency. The deprecated `@oxc-parser/wasm` package is not used.                                                                                                                                              | TypeScript Compiler API (rejected: heavy and editor-specific); Node `oxc-parser` (rejected: duplicates daemon logic); `@oxc-parser/wasm` (rejected: deprecated); Regex (rejected: fails on multi-line and complex syntax)                        |
| D-006 | papaya over DashMap for in-memory cache                                                   | papaya is lock-free and deadlock-safe. DashMap uses sharded RwLock which can deadlock when holding references. The import size workload is read-heavy after initial warmup.                                                                                                                                                                                                                                                | DashMap (rejected: locking semantics risk for read-heavy pattern)                                                                                                                                                                                |
| D-007 | redb over sled for persistent cache                                                       | redb hit 1.0 stable with a committed stable file format. sled has never shipped 1.0 and its on-disk format remains unstable.                                                                                                                                                                                                                                                                                               | sled (rejected: not stable); rusqlite/SQLite (viable but adds a C FFI dependency)                                                                                                                                                                |
| D-008 | Three compression formats (gzip, brotli, zstd)                                            | All three are in common production use as of 2026. CDNs serve all three. Running them in parallel with nested rayon::join adds negligible latency.                                                                                                                                                                                                                                                                         | Gzip only (rejected: brotli and zstd offer meaningfully better ratios); Brotli only (rejected: zstd is now mainstream)                                                                                                                           |
| D-009 | Platform-specific VSIX distribution                                                       | Users download only the binary for their own platform. Each VSIX is 10-13 MB rather than a single 120+ MB universal package.                                                                                                                                                                                                                                                                                               | Universal VSIX (rejected: unacceptable total size); Runtime download of daemon binary (rejected: requires network at activation)                                                                                                                 |
| D-010 | Custom module graph walker over Rolldown embedding                                        | **Superseded by D-017.** At the time, Rolldown did not expose an embeddable Rust crate (C-003). The custom walker built from `oxc_parser` + `oxc_resolver` + `oxc_semantic` served until its structural correctness defects motivated the bundler redesign.                                                                                                                                                                | Rolldown Rust API (rejected at the time: unstable); Skip tree-shaking (rejected: inaccurate sizes for named imports)                                                                                                                             |
| D-011 | Hybrid inline rendering                                                                   | VS Code native inlay hints are accessible, provide reliable size-label hovers, and integrate with editor controls, but the API cannot assign arbitrary colors per hint. Import Lens therefore defaults to decoration-backed colored inline hints through `importLens.inlineRenderer: "colored"` for confidence visibility, while keeping native inlay hints available for users who prioritize screen-reader accessibility. | Native InlayHints only (rejected: no per-hint confidence colors); colored decorations only (rejected: weaker accessibility); end-of-line decorations only (rejected: less inline and less accessible); CodeLens only (rejected: takes full line) |
| D-012 | TypeScript 7.x over TypeScript 6.x                                                        | TS 7.0 is the native Go-based compiler and the current stable release. Import Lens adopted TS 6 as the deliberate bridge: modern tsconfig defaults, explicit ambient type inclusion (`types: ["node", "vscode"]`), and no legacy patterns. That bet paid off — moving to 7 needed only the `devDependency` bump, with `tsc --noEmit` clean and no source or `tsconfig.json` change.                                        | TypeScript 6.x (rejected: superseded; kept only as the migration bridge). TypeScript 5.x (rejected: legacy defaults, would require migrating through 6.x anyway)                                                                                 |
| D-013 | `request_id` field in BatchRequest/BatchResponse for cancellation                         | Timing-based heuristics for discarding stale responses are fragile when two requests are fired within milliseconds of each other. An explicit monotonic ID makes the discard decision unambiguous at zero protocol cost.                                                                                                                                                                                                   | Timing-only approach (rejected: race condition on fast edits); sequence number on daemon side only (rejected: daemon has no state to track which request is current)                                                                             |
| D-014 | `CacheInvalidateAll` as a distinct message type                                           | Sending one `CacheInvalidate` per package when `node_modules` is deleted would produce hundreds of IPC messages in a large project. A single bulk message is more efficient and avoids buffer pressure on the socket. The 20-package threshold is a pragmatic cutoff; below it, per-package messages give the daemon more granular invalidation information.                                                               | Always use bulk (rejected: loses granularity for small changes); always use per-package (rejected: floods socket on full reinstall)                                                                                                              |
| D-015 | Extension-side insight enrichment over daemon protocol expansion                          | Git diff state, VS Code globalState history, and UI-only barrel warnings are editor-context features. Keeping them in the extension avoids changing the native protocol for data the daemon cannot independently know and keeps daemon cache identity stable.                                                                                                                                                              | Add fields to `ImportResult` for every insight (rejected: daemon lacks editor/Git context); compute all insights in the daemon (rejected: would require Git and VS Code storage access in Rust)                                                  |
| D-016 | Clipboard named-import candidates over automatic namespace rewrites                       | Rewriting `import * as ns` safely requires semantic usage rewriting across the file, including property accesses and potential shadowing. The v1 feature enumerates exports and copies a candidate import while leaving code changes under user control.                                                                                                                                                                   | Automatic rewrite CodeAction (rejected: unsafe without full semantic transform); no action (rejected: misses a high-value tree-shaking improvement path)                                                                                         |
| D-017 | Rolldown embedding over the custom module graph walker (2026 bundler redesign)            | Rolldown began publishing an embeddable Rust crate, and the custom walker had accumulated structural correctness defects (dangling generated bindings, dropped effectful initializers, silently merged ambiguous star exports, empty external re-export bundles) rooted in three hand-enumerated decisions that nothing forced to agree. Rolldown 1.1.5 passed every qualification gate (construct matrix, pinned real packages, absolute latency/memory/determinism) on 2026-07-11 and was ~1.9x faster than the legacy engine. Its unguaranteed Rust API is contained by exact pins, the coordinated compiler-stack updater with a generated graph fingerprint, one narrow adapter, and requalification on every bump. | Keep fixing the custom walker (rejected: each fix surfaced defects its predecessor masked); custom reference-closure/fixpoint redesign (rejected: permanently owns bundler semantics); esbuild (oracle only: no supported Rust embedding); SWC bundler (rejected: second compiler stack, caller-owned glue); Rspack/Farm (rejected: integration scope) |

---

## 16. Appendix C: Technology Watch

This table tracks components that are currently used with known limitations, or where a better alternative exists but is not yet stable enough for production use. Each item should be re-evaluated at the specified cadence.

| Component                                 | Current State                                                                                                                                                  | Watch For                                                                                                                                          | Impact on Import Lens                                                                                                                                                                                                              | Re-evaluate           |
| ----------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------- |
| `oxc_minifier`                            | Stable 0.x release line, currently resolved to 0.139.0. Produces 1-2% variance from SWC.                                                                       | New OXC releases; minifier API or output changes.                                                                                                  | Upgrade OXC crates as a coordinated batch; re-run integration suite to confirm no accuracy regressions.                                                                                                                           | Every OXC release     |
| `oxc_resolver`                            | Currently resolved to 11.23.0. Separate repository (`oxc-project/oxc-resolver`), versioned independently from the OXC monorepo. Currently on major version 11. | Major version bump (e.g. 12.x); breaking changes to `ResolverOptions` or the `resolve()` API.                                                      | May require `Cargo.toml` update and code changes in `resolver.rs`. Upgrade separately from the OXC monorepo batch and run integration suite before merging.                                                                       | Each release          |
| Rolldown Rust API (`rolldown`)            | **Adopted.** v1.1.5 embedded as the only semantic bundler behind the engine adapter; the custom module graph walker and reachability code were deleted at the Phase 3 cutover, exactly as this row predicted. The Rust API still carries no semver guarantee.  | Upstream Rust-API changes on every release; a published Rust-API stability commitment; any release that exposes retention decisions in the public build output (which would let the static fallback drop its retained matcher — see FR-021/I9). **Note:** the previously-listed "Windows `sideEffects`-glob matching defect" was **refuted on 2026-07-12** and does not exist — Rolldown matches string and array globs correctly on Windows; the original fixture never reached the matcher. Do not reinstate that watch item.                                                                                          | Every version bump re-runs the bundler-redesign qualification gates through `pnpm deps:update:compiler`; the OXC cadence is bounded by Rolldown releases (C-003).                                    | Every rolldown release |
| `wasm32-wasip1-threads`                   | Experimental Rust/LLVM target. Deferred v1.1 candidate; not a v1.0 runtime path.                                                                               | WASI Preview 2 / Component Model threading (`wasm32-wasip2`). The `wasi-threads` proposal is legacy; `shared-everything-threads` is the successor. | May require retargeting before a future WASM fallback ships.                                                                                                                                                                      | Semi-annually         |
| `@vscode/wasm-wasi-core`                  | Supports WASI Preview 1 with experimental thread support. Deferred v1.1 candidate dependency.                                                                  | WASI Preview 2 support, Component Model integration, improved `SharedArrayBuffer` ergonomics.                                                      | Better thread reliability and broader environment support, subject to VS Code Desktop and Web limitations.                                                                                                                        | Semi-annually         |
| Rust `oxc_parser`                         | Stable 0.x release line, currently resolved to 0.139.0. Used by the daemon for document import extraction and for parsing the engine's linked chunk before validation and minification. | OXC module-record API changes; parser diagnostics or span behavior changes.                                                                        | Upgrade OXC crates as a coordinated batch and re-run daemon import parity, engine, and package analysis tests.                                                                                                                     | Every OXC release     |
| `papaya`                                  | v0.2.4. Pre-1.0 but actively maintained. Uses seize-based GC.                                                                                                  | 1.0 stable release; API changes to pinning semantics.                                                                                              | Minor migration effort if pinning API changes. Lock-free design is correct for the workload.                                                                                                                                      | Semi-annually         |
| VS Code Inlay Hints API                   | Stable. Used as an optional display mode.                                                                                                                      | Enhanced styling support (colors, icons), positioning improvements.                                                                                | Richer size display within inlay hints. Currently limited to plain text.                                                                                                                                                          | With VS Code releases |
| `redb`                                    | v4.x stable. ACID, pure Rust.                                                                                                                                  | Major version bumps; potential API changes.                                                                                                        | Migration effort proportional to API surface changes. File format is committed stable. Cache schema versioning (FR-026a) ensures seamless upgrades.                                                                               | Annually              |
| TypeScript 7.x ("Corsa")                  | Adopted. v7.0.2, the native Go-based compiler rewrite by Microsoft. Migrated from the TS 6.x bridge release.                                                   | Diagnostics that differ from the TS 6 checker; `tsdown` compatibility with the native compiler; ecosystem type packages that still assume TS 6.    | Adoption cost was the `devDependency` bump alone: `tsc --noEmit` clean, `tsdown` builds, no source or `tsconfig.json` change, exactly as this table predicted.                                                                     | Adopted               |
| VS Code engine version (`engines.vscode`) | Currently `^1.90.0`. All required APIs (InlayHintsProvider, FileSystemWatcher, TelemetryLogger, etc.) available at this version.                               | New stable APIs that would benefit Import Lens: richer decoration API, improved CodeLens rendering, enhanced inlay hint styling.                    | Raise `engines.vscode` and `@types/vscode` in tandem. Any bump excludes users on older VS Code versions and third-party forks; evaluate installed-base data before bumping.                                                       | Annually              |
