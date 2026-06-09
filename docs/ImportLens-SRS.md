# Software Requirements Specification: ImportLens

**VS Code Import Size Analyzer**

| Field    | Value            |
| -------- | ---------------- |
| Version  | 1.8              |
| Date     | 5 June 2026      |
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

This Software Requirements Specification defines the requirements for ImportLens, a Visual Studio Code extension that calculates and displays the real-world bundle cost of npm package imports directly inside the editor. The document covers functional behaviour, system architecture, technical stack decisions, performance requirements, and distribution constraints.

The primary audience is the engineering team responsible for building and maintaining the extension.

### 1.2 Scope

ImportLens analyses import statements in JavaScript and TypeScript files and shows, inline next to each import, the actual post-tree-shake, minified, and compressed byte size that the import would add to a production bundle. The extension also surfaces bundle-impact insights such as working-tree import deltas, shared dependency explanations, package history trends, and tree-shaking opportunity actions. The extension does this without running the user's build system, without modifying any project files, and without blocking the editor.

The system performs real tree-shaking and minification inside a background Rust daemon process. Results are cached persistently so that repeat lookups are instant. The extension works for any project that uses npm packages, regardless of which bundler the project itself uses.

**Out of scope for v1.0:**

- Local relative imports (e.g. `import { util } from './helpers'`)
- CSS, image, or other non-JS/TS asset imports
- Monorepo cross-package imports where the dependency is not published to npm
- Support for Yarn Plug-n-Play (PnP) without `node_modules`

### 1.3 Definitions and Acronyms

| Term         | Definition                                                                                                     |
| ------------ | -------------------------------------------------------------------------------------------------------------- |
| OXC          | The Oxidation Compiler, a suite of high-performance JS/TS tools written in Rust                                |
| VSIX         | Visual Studio Extension package, the distribution unit for VS Code extensions                                  |
| IPC          | Inter-Process Communication, the channel between the extension host and the Rust daemon                        |
| MessagePack  | A binary serialization format used as the IPC encoding layer                                                   |
| Unix socket  | A POSIX IPC endpoint used on macOS and Linux                                                                   |
| Named pipe   | The Windows equivalent of a Unix socket, used for IPC on Win32 targets                                         |
| Tree-shaking | Dead code elimination that retains only the symbols actually used by an import                                 |
| redb         | An embedded, ACID-compliant key-value database written in pure Rust                                            |
| papaya       | A lock-free concurrent hash map crate for Rust                                                                 |
| WASM         | WebAssembly, a portable binary instruction format                                                              |
| WASI         | WebAssembly System Interface, the ABI for running WASM outside a browser                                       |
| ESM          | ECMAScript Modules, the static module format required for effective tree-shaking                               |
| LTO          | Link-Time Optimization, a compiler setting that reduces Rust binary size                                       |
| NAPI         | Node-API, the stable ABI for building native Node.js addons; used by `oxc-parser` for high-performance parsing |
| NAPI-RS      | A framework for building compiled Node.js addons in Rust                                                       |
| SRS          | Software Requirements Specification                                                                            |
| FR           | Functional Requirement                                                                                         |
| NFR          | Non-Functional Requirement                                                                                     |
| AST          | Abstract Syntax Tree                                                                                           |
| CJS          | CommonJS, the older Node.js module format that does not support static tree-shaking                            |

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

ImportLens is a standalone VS Code extension. It does not replace or wrap any existing extension. It complements bundler tooling (Vite, webpack, Rolldown, etc.) by surfacing import cost information at authoring time rather than after a build.

Unlike existing calculators that spin up Node.js bundlers, ImportLens offloads all heavy computation to a decoupled Rust background process. This guarantees editor stability and minimal memory overhead inside the extension host. The daemon protocol is kept behind a transport boundary so a future WebAssembly worker can reuse it, but v1.0 ships native daemon binaries only.

The extension introduces a background native process (the Rust daemon) which runs separately from the VS Code extension host. This separation is a deliberate design choice: the extension host is a shared Node.js process that also runs every other installed extension. Placing CPU-intensive work (parsing, tree-shaking, compression) inside the extension host would degrade the entire editor. The daemon runs in its own process with its own memory space, and a crash in the daemon does not affect VS Code. When a supported file is opened outside a VS Code workspace folder, the extension derives an analysis root by walking upward from the file to the nearest `package.json` or `node_modules` directory and still resolves packages from the active document path.

### 2.2 Product Functions

At a high level, ImportLens:

1. Detects import statements in the currently active JS/TS file
2. Filters to node_modules imports only
3. Resolves the installed version of each package from the project's node_modules
4. Sends a batched request to the background Rust daemon over a local socket
5. Receives computed size data (raw, minified, and compressed) for each import
6. Renders the size inline in the editor as confidence-styled inline hints by default, native accessible inlay hints, end-of-line decorations, or code lens annotations
7. Adds contextual insights such as Git working-tree deltas, per-import history trends, shared-byte explanations, and barrel re-export warnings
8. Provides commands for current-file totals, bundle impact history, workspace reports, diagnostic copying, and cache clearing
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
- The extension host component must be written in TypeScript 6.x and compiled to a single bundled JS file using `tsdown`. The minimum supported VS Code version is 1.90.0, declared via `"engines": { "vscode": "^1.90.0" }` in `package.json`. This version provides a modern baseline while ensuring compatibility with AI-focused VS Code forks (such as Cursor, Windsurf, and Antigravity) that often lag several months behind upstream releases.
- The `tsconfig.json` must use TypeScript 6.x conventions: `module: "esnext"`, an explicit `types` array (not auto-include; currently `["node", "vscode"]`), `moduleResolution: "bundler"`, and `target: "es2025"`. Legacy module formats (`amd`, `umd`, `systemjs`) and legacy `moduleResolution: "node"` (Node10) must not be used.
- The native daemon must be compiled separately for each target platform and distributed as a platform-specific VSIX.
- The published VSIX for any single platform target must not exceed 20 MB.

### 2.6 Assumptions and Dependencies

- The user's project has a `node_modules` directory populated by a package manager (npm, yarn, or pnpm with hoisting).
- Each importable package has a `package.json` in its `node_modules/<package>/` directory. A parseable string `version` field enables exact cache identity; malformed or versionless manifests are still requestable and fall back to approximate package-directory sizing.
- Packages that expose ESM entry points (via the `exports` or `module` field in `package.json`) will produce accurate tree-shaken sizes. CommonJS-only packages are analyzed statically where possible and produce approximate sizes with a visible warning when the daemon must fall back.

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
│  │           │ oxc-parser (NAPI)     │ sizes       │   │
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
If a native binary is unavailable or cannot be verified, or if the environment is VS Code for the Web where local `node_modules` access is unavailable, the extension operates in degraded mode. Import statements are detected where the extension host can parse them, but no size computation is performed. The UI shows a status bar indicator explaining that full analysis is unavailable.

**Post-v1 Candidate - WASM Desktop Fallback:**
A WebAssembly daemon fallback may be added in v1.1 or later using the existing analysis transport boundary. It is not a v1.0 runtime path and must not be advertised or packaged until the `wasm32-wasip1-threads` build, VS Code Worker execution model, and release pipeline are proven end-to-end. See constraint C-004 in Section 13.1.

### 3.3 Startup Sequence

1. Extension activates on the `onLanguage:javascript`, `onLanguage:typescript`, `onLanguage:typescriptreact`, `onLanguage:javascriptreact`, `onLanguage:json`, `onLanguage:jsonc`, `onLanguage:svelte`, `onLanguage:astro`, and `onLanguage:vue` events.
2. The extension host checks for a native binary matching the current platform in the extension's `bin/` directory.
3. If found, it verifies the binary's SHA-256 hash against the known-good hash embedded in the extension package (NFR-014a). If the hash does not match, the extension logs a security warning and enters degraded mode.
4. If the hash matches, it spawns the daemon process, pipes daemon stdout/stderr into the ImportLens output channel according to the configured log level, opens a socket connection, and sends a versioned `HelloMessage`. The socket path includes a window-unique identifier (NFR-014b).
5. If no native binary is found, or if the binary cannot be verified, spawned, connected, or sent a hello message, the extension disposes any partial IPC client state, terminates the spawned daemon process when it is still alive, and enters the restart/degraded-mode path defined in FR-015.
6. The daemon opens its persistent `redb` cache from the VS Code global storage directory, verifies the schema version (FR-026a), and preloads only a bounded set of recent valid size entries into the in-memory `papaya` cache.
7. The extension is ready to accept requests.

### 3.4 Request Lifecycle

On each daemon respawn, the extension host reads `<globalStoragePath>/importlens-recycles.json` before deciding whether to spawn, applying the recycle rate limit defined in NFR-004b.


1. The user opens or edits a supported JS/TS, JSX/TSX, Svelte, Astro, or Vue file.
2. The document listener fires after a 300ms debounce.
3. The extension extracts parseable script regions for component files. Plain JS/TS and JSX/TSX files are parsed as one region; Svelte `<script>` blocks and Vue `<script>` / `<script setup>` blocks are parsed as component regions; Astro frontmatter is parsed as server runtime and processed Astro `<script>` blocks are parsed as client runtime.
4. `oxc-parser` (NAPI binding) parses each script region and extracts ESM import information directly from the `result.module.staticImports`, `result.module.staticExports`, and `result.module.dynamicImports` arrays (no AST walk required).
5. The extension maps region-relative import ranges back to absolute document positions for stable inlay hint, decoration, and CodeLens placement.
6. The extension filters imports to node_modules only, skipping local paths, `node:` builtins, and `import type` declarations.
7. For each remaining import, the extension reads the installed package version from `node_modules/<package>/package.json`. For scoped packages (e.g. `@babel/core`), the path includes the scope directory. If the package directory exists but the manifest is malformed or lacks a string `version`, the extension sends the request with an unknown-version sentinel so the daemon can return an approximate fallback instead of marking the import missing.
8. A single batched `BatchRequest` is serialised to MessagePack and sent over the socket. Protocol v2+ clients may set `streaming: true` to receive indexed partial responses as individual imports finish.
9. The daemon receives the batch, verifies that the connection has completed the `HelloMessage` handshake, and checks its `papaya` map for each import's cache key.
10. Cache hits are returned immediately. Cache misses are fanned out to a Rayon thread pool for parallel processing.
11. For each miss, the daemon runs the OXC pipeline: (a) resolve the package entry point via `oxc_resolver`, (b) build the module graph by recursively parsing reachable relative and bare transitive imports with `oxc_parser` and analysing them with `oxc_semantic`, (c) transform TypeScript and JSX modules with `oxc_transformer`, (d) concatenate reachable code or a conservative parsed graph when side-effect metadata requires it, (e) run `oxc_minifier` for dead code elimination and mangling, (f) emit the minified string via `oxc_codegen` using the minifier-provided scoping and private-member mappings, and (g) compress in parallel with `flate2`, `brotli`, and `zstd` using nested `rayon::join` calls.
12. Results are written to `papaya` (memory) and `redb` (disk).
13. The daemon serialises one full `BatchResponse` for protocol v1/full-batch requests, or writes indexed partial `BatchResponse` frames to the socket as individual imports finish followed by a final full response for protocol v2+ streaming requests.
14. The extension host deserialises responses, discards stale `request_id` values, and updates decorations progressively without regressing newer results.
15. When the final response for a document is current, the extension enriches ready states with extension-side insights: Git working-tree import deltas, per-import history trends, shared-module explanations, and barrel re-export warnings.
16. The extension records bounded per-import and current-file history entries in VS Code global storage. History persistence failures are logged but must not mark an otherwise successful size result unavailable.

---

## 4. Architectural Alternatives and Rationale

This section documents the key architectural decisions made before implementation and the alternatives that were evaluated. The primary constraint driving all decisions was a hard 20 MB per-platform VSIX size limit.

### 4.1 Bundler and Pipeline Selection

**Evaluated:** Rspack, Rolldown, ESBuild, and OXC.

**Rspack and Rolldown rejected:** Both are Rust-powered tools, but they expose Node.js APIs rather than embeddable Rust crates. Using either would require spawning an additional Node.js subprocess from within the Rust daemon, which eliminates the performance and isolation advantages of writing the daemon in Rust.

**ESBuild rejected:** ESBuild is written in Go and requires managing a separate WASM execution layer to use programmatically from Rust. This adds complexity and an additional binary dependency.

**OXC selected:** OXC provides pure, embeddable Rust crates (`oxc_parser`, `oxc_resolver`, `oxc_transformer`, `oxc_minifier`) that compile into a single binary. All pipeline stages share the same in-memory AST, eliminating re-parsing between steps. OXC is the engine used internally by Rolldown and Vite 8.

### 4.2 Minifier Selection

**Evaluated:** SWC Core, Terser, and OXC Minifier.

**Terser rejected:** Terser is a JavaScript tool and would require a Node.js subprocess from within the Rust daemon, contradicting the native-first architecture.

**SWC Core rejected:** SWC produces slightly better compression ratios but its platform-specific binary is approximately 25 to 27 MB depending on the target. Including SWC would push every platform VSIX over the 20 MB hard limit.

**OXC Minifier selected:** It is part of OXC's stable 0.134.x toolchain. The 0.x version number does not indicate alpha quality; it reflects the Rust and npm package versioning scheme used before a 1.0 line. Minified output may vary by 1 to 2 percent from SWC, which is acceptable for a size estimation tool. See Section 13.1 for the upgrade policy.

### 4.3 Extension-Side Parsing

**Evaluated:** Regular expressions, TypeScript Compiler API, and OXC WASM Parser.

**Regular expressions rejected:** They fail on multi-line imports, re-exports, and complex TypeScript syntax patterns.

**TypeScript Compiler API rejected:** It introduces heavy initialization overhead, requires the `typescript` npm package as a runtime dependency, and does not work in VS Code for the Web.

**OXC Parser selected (NAPI):** `oxc-parser` is the actively maintained OXC parser package for Node.js. It uses native NAPI bindings for high performance. The deprecated `@oxc-parser/wasm` is not used because it is officially unmaintained. The NAPI parser returns ECMAScript module information directly in structured module-record arrays such as `result.module.staticImports`, `result.module.staticExports`, and `result.module.dynamicImports` without requiring an AST traversal. On VS Code Desktop, it is loaded as a native dependency. In VS Code for the Web (degraded mode), parsing is unavailable as NAPI bindings do not run in browser environments.

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

The extension must retain the detected import syntax category (`static`, `reexport`, `star_reexport`, or `dynamic`) in its in-memory analysis state so UI features can distinguish normal namespace imports from barrel re-export boundaries without relying on daemon heuristics.

**FR-002** (Critical) - The extension must skip relative imports (those beginning with `./` or `../`).

**FR-003** (Critical) - The extension must skip Node.js built-in module imports, including those prefixed with `node:` and those matching known built-in names such as `fs`, `path`, `os`, `http`, and `crypto`.

**FR-003a** (High) - The extension must skip framework virtual modules and common application aliases that are not npm package dependencies, including `astro:*`, `virtual:*`, `$app/*`, `$env/*`, `$lib/*`, `#imports`, `@/*`, and `~/*`.

**FR-004** (High) - The extension must use `oxc-parser` (NAPI binding, npm package `oxc-parser`) to parse import information from the document buffer via the synchronous `parseSync()` function. It must not use the TypeScript Compiler API or spawn a separate process for parsing. The deprecated `@oxc-parser/wasm` package must not be used.

**FR-005** (High) - The extension must use OXC parser module-record output to extract imports directly from `staticImports`, `staticExports`, and `dynamicImports`. When OXC returns recoverable module information while the user is mid-typing, the extension must extract as much structural information as possible. If the parser cannot produce usable module information, the extension must skip that debounce cycle without showing a blocking error.

**FR-006** (Critical) - The extension must debounce parse-and-request operations by the value configured in `importLens.debounceMs` (default 300ms) after the last document change event. Requests must not be sent on every keystroke.

**FR-006a** (Critical) - The extension must support Svelte documents by extracting imports from every `<script>` block, including module-context and instance scripts. `<script lang="ts">` blocks must be parsed as TypeScript and all detected import positions must map back to the original `.svelte` document.

**FR-006b** (High) - The extension must support Astro documents by extracting imports from frontmatter and processed client `<script>` blocks. Frontmatter imports must be marked as `server` runtime; processed client script imports must be marked as `client` runtime. Inline Astro scripts with non-processed attributes such as `is:inline` must not be treated as bundled imports.

**FR-006c** (High) - The extension must support local JS/TS files opened outside a VS Code workspace folder. For such loose files, the extension must derive an analysis root by walking upward from the file to the nearest `package.json` or `node_modules` directory and must start the daemon with that derived root. If neither exists, the file's containing directory is used as the fallback root. Loose-file support must use the active document path for package resolution and must not display daemon unavailable solely because no workspace folder exists.

**FR-006d** (High) - The extension must support Vue Single File Components by extracting imports from every `<script>` block, including `<script setup>` and classic scripts. `<script lang="ts">`, `<script lang="tsx">`, and `<script lang="jsx">` blocks must be parsed with the matching language mode, and all detected import positions must map back to the original `.vue` document.

### 5.2 Package Version Resolution

**FR-007** (Critical) - Before sending a request to the daemon, the extension must resolve each package by reading `node_modules/<package>/package.json` and extracting the `version` field when it is present as a string. For scoped packages (e.g. `@babel/core`), the path is `node_modules/@<scope>/<name>/package.json`. The `<package>` identifier in all cache keys and IPC messages includes the full scope prefix when present. If the package directory exists but the manifest is malformed or lacks a string `version`, the extension must still send the package to the daemon using an unknown-version sentinel so the daemon can compute the approximate fallback described in Section 7.1.

**FR-007a** (High) - The extension host package resolver must search upward from the active document path, not from the first workspace folder. This mirrors Node resolution in nested workspaces and loose-file windows before the daemon receives the request.

**FR-008** (High) - The daemon resolver must start package discovery and module resolution from the `active_document_path` supplied in `BatchRequest`, not from the workspace root. Starting from the file being edited ensures that upward traversal through the directory tree matches Node's own resolution algorithm exactly. This is critical in multi-root VS Code windows, NPM Workspaces, Yarn Workspaces, and nested PNPM layouts where a package inside `packages/app-a/` may have its own `node_modules/` with a different version of a dependency than the root-level hoisted copy. The daemon must validate package identifiers before building filesystem paths and must reject identifiers containing traversal or platform path separators.

**FR-009** (High) - If a package cannot be found in `node_modules`, the extension must display a subtle "Package not found" decoration on that import line and must not send it to the daemon. This missing-package path applies only when the package directory cannot be located; installed packages with malformed or versionless manifests follow FR-007's daemon fallback path.

### 5.3 Daemon Communication

**FR-010** (Critical) - The IPC protocol must use MessagePack as the serialization format on both the TypeScript and Rust sides.

**FR-011** (Critical) - Messages must be length-prefixed with a 4-byte big-endian unsigned integer representing the byte length of the MessagePack payload that follows. This allows the socket to handle concurrent in-flight requests without message boundary ambiguity. Both the TypeScript and Rust decoders must reject frames larger than 32 MiB and must validate frame length arithmetic before allocating a payload buffer.

**FR-012** (Critical) - The extension must send all imports from a single debounce cycle as a single `BatchRequest`, not one request per import line.

**FR-013** (High) - The extension must implement request cancellation using the `request_id` field present in both `BatchRequest` and `BatchResponse`. Each debounce cycle must increment a monotonic counter and send it as `request_id`. If a response arrives whose `request_id` does not match the most recently sent request, the extension must discard it without updating decorations. This makes cancellation unambiguous regardless of response timing; timing-based heuristics must not be used.

**FR-013a** (High) - When the daemon encounters a computation error for one or more imports in a batch, it must return a partial `BatchResponse` containing successful results for all other imports in the same batch. For each failed import, the `ImportResult.error` field must be set to a non-null string describing the failure reason, `ImportResult.diagnostics` must include at least one structured diagnostic entry with the failing stage and real daemon context, and all numeric size fields must be set to `0`. The extension host must render a subtle "Size unavailable" decoration for imports whose `ImportResult.error` is non-null, and must not show a user-visible error dialog. The extension host must keep raw diagnostic details out of the inline UI while making them copyable from the hover.

**FR-013b** (High) - Protocol v2+ clients may request streaming batch responses by setting `BatchRequest.streaming: true`. In streaming mode, the daemon must emit partial `BatchResponse` frames as import results become available and set `BatchResponse.indexes` to the zero-based import indexes represented by that frame. The IPC server must write each partial frame to the socket while the rest of the batch is still computing; it must not buffer all partials in memory and flush them only after the final result is ready. This index list is required because duplicate specifiers can appear multiple times in one file. A final full-batch `BatchResponse` with shared-byte annotations must still be emitted for compatibility with existing request-state handling. Protocol v1 clients and v2+ clients without `streaming: true` receive only a full batch response.

**FR-014** (High) - On socket disconnect, the extension must discard any stale MessagePack payloads currently in the receive buffer and wait for the next document change event to trigger a fresh request cycle.

**FR-015** (High) - If the daemon process crashes, the extension must detect the disconnection, wait 1 second, and attempt to restart the daemon. On subsequent failures, it must apply exponential backoff (1s, 2s, 4s, 8s, capped at 30s). After three consecutive failures within 60 seconds, it must enter degraded mode and display a status bar notification.

**FR-015a** (High) - The extension host must pipe daemon stdout to the ImportLens output channel at info level and daemon stderr at warning level. The logger's configured level controls visibility, and the default log level is `info` so the status-bar "Show Logs" path contains useful startup diagnostics without extra configuration. Failed startup after process spawn, including IPC connect failure or hello-send failure, must dispose any created IPC client and terminate the child daemon process before scheduling restart or entering degraded mode.

### 5.4 Size Computation

**FR-016** (Critical) - For each cache-miss import, the daemon must construct a virtual ESM entry file in memory using re-export semantics:
- Named imports: `export { <namedExports> } from '<package>'`
- Default imports: `export { default } from '<package>'`
- Namespace imports: `export * from '<package>'`
- Dynamic imports: resolve the package entry point directly without a virtual file

The virtual entry must never use `console.log` or any pattern that can be statically eliminated by a tree-shaker.

**FR-017** (Critical) - The daemon must use `oxc_resolver` to resolve the package entry point from `node_modules`. The resolver must use the following `exports` condition set, in priority order: `["module", "import", "default"]`. This selects the ESM path when available, which is required for accurate tree-shaking. The `"require"` condition must not be in the set; its presence would cause `oxc_resolver` to prefer CJS paths on packages that publish both. If no ESM entry can be resolved, the daemon falls back to the `"main"` field and sets `is_cjs: true` in the response. The resolver must also respect the `"browser"` field for packages that use it as an ESM entry alias. The `"module"` top-level field (used by older packages before the `exports` map existed) is respected as a lower-priority fallback after `exports` map resolution. During module graph construction, every relative and bare transitive ESM import must be resolved from the importing module's path with the same resolver semantics, including TypeScript source aliases for emitted `.js`, `.mjs`, `.cjs`, and `.jsx` specifiers when matching `.ts`, `.mts`, `.cts`, and `.tsx` source files exist. Node builtins, unresolved peers, and other externals must remain outside the graph and must produce structured diagnostics rather than failing the whole import when partial analysis can continue.

**FR-017a** (High) - If package entry resolution fails but the installed package directory contains declaration files (`.d.ts`, `.d.mts`, or `.d.cts`) and no runtime JavaScript or TypeScript source files (`.js`, `.mjs`, `.cjs`, `.jsx`, `.ts`, `.tsx`, `.mts`, or `.cts`, excluding declaration files), the daemon must return a successful zero-byte `ImportResult` instead of marking the import unavailable. The result must set all byte fields to `0`, `side_effects: false`, `is_cjs: false`, and include a structured `types_only` diagnostic so the extension can label the import as declaration-only runtime cost.

**FR-018** (Critical) - The daemon must perform tree-shaking using a custom module graph walker built on OXC primitives. The pipeline is:
1. Construct a virtual ESM entry module (as defined in FR-016).
2. Resolve the package entry point via `oxc_resolver`.
3. Recursively parse all reachable modules using `oxc_parser`, building the module graph. Graph construction must enforce hard limits of 2,000 modules, 5 MiB per module source file, and 50 MiB total graph source bytes.
4. Run `oxc_semantic` on each parsed module to produce scope trees, symbol tables, and binding information.
5. Walk the module graph from the virtual entry's requested exports, marking all transitively reachable code.
6. Concatenate only the reachable code into a single in-memory source.
7. Before concatenating reachable code, the daemon must run `oxc_transformer` on each parsed module to strip TypeScript types and transform JSX. This produces plain JavaScript ASTs that can be processed by `oxc_minifier`. `oxc_transformer` does NOT perform tree-shaking; it only handles syntax lowering.
8. When concatenating reachable modules into a single source, the daemon must apply scope renaming to prevent collisions between identically-named bindings in different module scopes (e.g. two modules both declaring `const x = ...`). Renaming must be based on semantic binding and reference spans, not ad hoc string replacement, and must preserve object shorthand, object destructuring, array destructuring, and rest binding semantics. See Section 10.7 for the module graph walk algorithm.
9. Circular dependency edges must be detected during graph construction and reported as `circular_dependency` diagnostics on affected import results. Cycles must not cause infinite traversal or duplicate module inclusion.

**FR-019** (Critical) - The daemon must use `oxc_minifier` to perform dead code elimination, constant folding, and supported identifier mangling on the tree-shaken output, then use `oxc_codegen` (with `minify: true`) to emit the minified JavaScript string. Codegen must use the scoping and private-member mappings returned by `oxc_minifier::Minifier::minify`; the daemon must not run a second independent mangling pass over already-minified AST state.

**FR-020** (Critical) - After minification, the daemon must compute three compressed sizes in parallel: gzip using `flate2` at level 6, Brotli using the `brotli` crate at level 4, and zstd using the `zstd` crate at level 3.

**FR-021** (Critical) - The daemon must read the `sideEffects` field from the package's `package.json` before tree-shaking. The field is handled as follows:
- If the field is `true` or absent: the response must set `side_effects: true`, include the full parsed graph for named/default imports, and set `truly_treeshakeable: false`.
- If the field is `false`: aggressive module pruning is permitted; the response sets `side_effects: false`.
- If the field is an array of glob patterns (e.g., `["*.css", "dist/polyfill.js"]`): the daemon must evaluate the patterns against the resolved package entry path using webpack-compatible `*`, `?`, `**`, and simple brace alternatives. If the entry matches, the response must set `side_effects: true`, include the full parsed graph, set `truly_treeshakeable: false`, and add a structured side-effect diagnostic. If the entry does not match, named/default ESM imports may be tree-shaken normally.

**FR-022** (High) - The daemon must detect when a package is not genuinely tree-shakeable by comparing the named-export size against the full-package size. If the named-export size is within 5% of the full-package size, `truly_treeshakeable` must be set to `false` in the response.

**FR-023** (High) - The daemon must process all imports in a single `BatchRequest` concurrently using a Rayon thread pool. The thread pool must be sized to `max(1, available_parallelism - 2)` to leave headroom for VS Code's renderer and extension host threads. This is configured via `rayon::ThreadPoolBuilder::new().num_threads(std::thread::available_parallelism().map(|n| n.get().saturating_sub(2).max(1)).unwrap_or(1)).build_global()`. The `num_cpus` crate must not be used; `std::thread::available_parallelism()` (stable since Rust 1.59) is the stdlib replacement and correctly respects cgroup limits.

**FR-024** (Critical) - The Rust daemon must operate exclusively via static AST analysis. It is prohibited from evaluating, executing, or interpreting any code found within third-party packages. No `eval`, subprocess execution, or dynamic code loading of any kind is permitted.

**FR-024a** (High) - CommonJS support must be implemented through static analysis only. For CJS entry points, the daemon may scan literal relative `require()` calls and common export shapes such as `exports.foo`, `exports["foo"]`, `module.exports.foo`, `module.exports["foo"]`, `module.exports = { foo }`, and default-like `module.exports = function/class`. String, template, comment, and regex literal bodies must be masked before scanning so text that merely resembles `require()` is not treated as a dependency. Dynamic `require()`, unsupported export shapes, and unresolved CJS dependencies must fall back to conservative entry sizing with `cjs_fallback` or `cjs_resolution` diagnostics. File-level size requests that contain only CommonJS imports must return conservative non-deduped CJS totals with diagnostics instead of reporting zero bytes. The daemon must never use `oxc_transformer` as a CJS-to-ESM converter because the pinned OXC transformer does not provide that conversion path.

**Implementation status note (Windows alpha):** The current Windows alpha runs the OXC graph pipeline for ESM entries and uses the CommonJS static analyzer described in FR-024a for CJS entries. When static graph analysis cannot safely proceed, the daemon returns conservative static-entry estimates with structured diagnostics instead of throwing away partial successful results.

### 5.5 Caching

**FR-025** (Critical) - The daemon must maintain an in-memory cache using a `papaya::HashMap`. Cache keys must use the structured v3 identity format described in Section 10.2, including analyzer version, package identity, runtime profile, import kind, sorted named exports, resolved package paths when known, and relevant file fingerprints. Valid cache hits must be returned without running any computation.

**FR-026** (Critical) - When `importLens.enableDiskCache` is `true` (the default), the daemon must persist computed cache entries to a `redb` database stored in the VS Code global storage directory. On startup, the daemon must preload only the configured bounded recent-entry set into `papaya`; other valid disk entries remain available through lazy disk lookup and are promoted into memory on first hit.

**FR-026a** (High) - The `redb` database must include a metadata table containing a `schema_version` integer. The current schema version is `4`. On startup, the daemon must read this value before loading cache entries. If `schema_version` is missing or does not match the version expected by the current daemon binary, the daemon must delete the existing database file, create a fresh empty database with the current schema version, and log a warning. This ensures forward compatibility across daemon upgrades (including the redb v3→v4 major version migration and protocol-result shape changes).

**FR-026b** (Medium) - The daemon must track recent cache usage in persistent metadata so that startup can prewarm the most useful entries. Each successful disk insert must update the recent-entry timestamp for the corresponding cache key. Disk and memory hits may batch or debounce recency updates to avoid synchronous redb writes on every hot memory hit, but pending recency updates must be flushed during normal shutdown/drop. On handshake completion, the daemon must prewarm up to the 20 most recent valid entries after resolving them from the active workspace dependency tree.

**FR-027** (High) - The TypeScript extension host must watch `node_modules` for package version changes using VS Code's native `vscode.workspace.createFileSystemWatcher` API with two glob patterns: `**/node_modules/*/package.json` for regular packages and `**/node_modules/@*/*/package.json` for scoped packages (e.g. `@babel/core`). Both watchers must be registered at activation and both must send `CacheInvalidate` messages to the daemon when they fire. The `notify` Rust crate must not be used for this purpose. On Linux, a Rust process watching `node_modules` directly would register one `inotify` file descriptor per directory, which on kernels before 5.11 could rapidly exhaust the system-wide `inotify` limit (`fs.inotify.max_user_watches`, which defaulted to 8,192 prior to kernel 5.11). Since kernel 5.11 (February 2021), the default is dynamically scaled based on available memory (up to 1,048,576 on 64-bit systems with >=128 GB RAM), but the old default persists on older kernels and in constrained containers. Regardless of kernel version, VS Code's file watcher already manages file descriptor budgets safely for all extensions combined, making it the correct abstraction. When the watcher fires for a given package, the extension host must send a `CacheInvalidate` message over the existing IPC socket to the daemon. On receiving this message, the daemon must evict all cache entries for that package from both `papaya` and `redb`. When an entire `node_modules` directory is deleted or more than 20 packages are invalidated in a single burst, the extension host must send a single `CacheInvalidateAll` message instead of individual per-package messages. See Section 10.1 for the `CacheInvalidateAllMessage` schema.

**FR-028** (Medium) - When a user opens or saves a `package.json` file in the workspace, the daemon must pre-calculate and cache the sizes of the default export and the namespace export (`*`) for each dependency listed in that file's `dependencies` and `devDependencies` objects. These two export variants are the most common and cover the majority of real-world import patterns. Pre-warm tasks must run on a dedicated secondary Rayon thread pool configured with half the threads of the primary pool, so that the primary pool remains fully available for real user requests. Because Rayon does not expose OS-level thread priority, reduced pool size is the correct mechanism for deprioritisation. Pre-warm work must stop immediately when a real `BatchRequest` arrives or when the prefetcher is dropped during shutdown/recycle, and must reuse already-resolved package entries rather than resolving the same package twice.

### 5.6 User Interface

**FR-029** (Critical) - The extension must display size information inline by default through `importLens.display: "inlayHint"` and `importLens.inlineRenderer: "colored"`. The colored inline renderer is the default because it can apply the shared confidence color mapping directly beside each import. Native VS Code inlay hints remain available through `importLens.inlineRenderer: "native"` for users who prioritize screen-reader-accessible editor integration over per-result colors. End-of-line text decorations remain available via `importLens.display: "standard"` or `importLens.display: "verbose"` for users who prefer line-end annotations.

**FR-030** (Critical) - The display format must be configurable via `importLens.display` with four options:
- `minimal`: `1.5 kB` (primary compression format only)
- `standard`: `1.5 kB br · 5.3 kB min` (primary compression size and minified size)
- `verbose`: `1.5 kB br · 1.8 kB gz · 1.6 kB zstd · 5.3 kB min` (all three formats)
- `inlayHint`: Displays the primary compression size as an inline hint after the import specifier. Rendering is selected by `importLens.inlineRenderer`: `native` uses the VS Code Inlay Hints API, while `colored` uses decoration-backed inline text with confidence colors.

**FR-031** (High) - When `side_effects: true`, `is_cjs: true`, or `truly_treeshakeable: false`, the extension must display a warning indicator next to the size label indicating that the shown size may be conservative. The literal word `approximate` must not appear in inline size labels. Low-confidence size labels must use a leading `~`, for example `~1.6 kB br`. Medium- and high-confidence labels must not use `~`; confidence is conveyed through hover, report, diagnostic details, and confidence colors on surfaces that support per-result colors.

**FR-031a** (Medium) - When an import is detected from Astro frontmatter, the extension must label the displayed size with `server` and include the runtime in the hover tooltip so users do not confuse server-only dependency cost with client bundle cost.

**FR-031b** (Medium) - When the active file is tracked by Git and an import statement overlaps an added or modified line in the working-tree diff against `HEAD`, the extension must append a positive import-cost delta label based on the current import's Brotli bytes, for example `+2.1 kB br`. Deleted imports have no current editor range and are out of scope for inline labels.

**FR-031c** (Medium) - The extension must persist a bounded per-import size history in VS Code global storage. When a current import result differs from the most recent stored entry for the same import identity, the hover tooltip must include a trend note showing the previous Brotli size, current Brotli size, and signed delta.

**FR-031d** (Medium) - When multiple imports in the same file share module paths reported by `module_breakdown` and `shared_bytes`, the extension must add hover insight text naming up to three shared module basenames and the other specifiers that include them. If the daemon reports `shared_bytes` but the shared modules are outside the public top-module breakdown, the hover must still explain that shared bytes exist.

**FR-031e** (Medium) - When the parser detects a star re-export (`export * from "package"`), the extension must surface a barrel-boundary insight. The inline label may append `barrel`, and the hover must explain that the broad re-export can prevent precise named-export tree-shaking.

**FR-031f** (High) - The extension must use one shared confidence visual mapping across colored inline hints, report rows, treemap legends, and full detail surfaces: high confidence uses a muted success color, medium confidence uses an amber warning color, and low confidence uses an error red color. Full detail surfaces must emphasize key fields with Markdown structure: bold package name, bold selected compression size, a confidence badge or label, reason list, side-effect/CommonJS/tree-shakeability status, and a diagnostics command link when diagnostics are available.

**FR-032** (High) - The extension must display a loading indicator next to imports that are currently being computed (cache miss in progress).

**FR-033** (High) - The extension must provide a status bar item showing the daemon's current state: `ImportLens: Ready`, `ImportLens: Computing...`, or `ImportLens: Unavailable`.

**FR-034** (High) - Changing the `importLens.compression` setting must immediately update all currently visible inline decorations to reflect the new format selection without requiring a file change or editor reload.

**FR-035** (Medium) - The extension must provide a command `ImportLens: Clear Cache` that evicts all entries from both `papaya` and `redb` and triggers a fresh computation for the active document.

**FR-036** (Medium) - The extension must provide a command `ImportLens: Show Report` that opens a webview panel listing all imports in the workspace along with their sizes, sorted by brotli size descending. The report must include workspace summary metrics, duplicate import aggregation, duplicate/vendored module insights, a static SVG treemap sized by Brotli bytes, confidence legend colors, and a static shared-module table so users can quickly identify dominant dependencies without running scripts in the webview.

**FR-036a** (Medium) - Report rows and hover tooltips must surface file-level sharing information when the daemon returns it. `ImportResult.module_breakdown` contains the top 10 module contributors for an import. `ImportResult.shared_bytes` contains the number of raw module bytes shared with at least one other import in the same file. The report must expose both the top contributors and shared-byte value without changing the inline decoration format.

**FR-036c** (Medium) - Report rows, hover tooltips, and copied diagnostics must expose `ImportResult.confidence` and `ImportResult.confidence_reasons`. Low- and medium-confidence rows must be countable in the report summary and must include the reasons in the row warning text.

**FR-036d** (Medium) - The extension must provide named-import member completions for existing ESM import clauses. When the cursor is inside `import { ... } from "specifier"`, the completion provider must request `EnumerateExportsRequest` from the daemon and offer cached named exports from the resolved graph. Completion requests must be best-effort and must fail silently in degraded mode.

**FR-036e** (Medium) - The extension must provide a command `ImportLens: Show Current File Size` that sends a `FileSizeRequest` for the active file's runtime package imports, receives a deduplicated file-level total, displays the selected compression summary, and records the measurement in bundle impact history. The command must work for supported loose files using the same analysis-root derivation as FR-006c.

**FR-036f** (Medium) - The extension must provide a command `ImportLens: Show Bundle Impact History` that reads recent current-file measurements from VS Code global storage and opens a script-free static SVG history panel with timestamp, file path, import count, and byte details.

**FR-036g** (Medium) - The extension must provide CodeActions for imports whose current result is CommonJS, side-effectful, or not truly tree-shakeable. These actions must allow users to inspect existing ImportLens details or copy diagnostics. They must not automatically rewrite user source.

**FR-036h** (Medium) - For namespace imports whose result is not truly tree-shakeable, the extension must offer a CodeAction that enumerates named exports through `EnumerateExportsRequest`, lets the user select one or more export names, and copies a candidate named import statement to the clipboard. The action must not rewrite source automatically because namespace member usage requires semantic transformation outside the current static import-cost scope.

**FR-036i** (Medium) - The extension must support configured bundle budgets for per-import and per-file Brotli thresholds. Budget violations must appear as VS Code diagnostics, must be visible in inline/hover/report text, and must be counted in report summaries.

**FR-036j** (Medium) - The repository must provide an `importlens check` CLI path that analyzes files changed by `git diff` and exits non-zero when configured budgets are violated. The CLI must fail clearly for malformed budget configuration and must not require VS Code to be running.

**FR-036k** (Medium) - The extension must offer curated import substitution suggestions through CodeActions using only a local mapping file. Suggestions may copy or show alternatives and size context, but must not rewrite source automatically.

**FR-036l** (Medium) - When `importLens.enableRegistryHints` is enabled, the extension may fetch npm metadata for registry-based hints. The setting must default to `true`; network work must use short timeouts, cache positive and negative results in VS Code globalState and session memory, and fail silently without affecting size computation.

**FR-036m** (Medium) - When a `package.json` file is open, the extension must provide compact dependency-cost inlay hints for dependency blocks using local package resolution and prewarm-friendly daemon requests. Rendering must read from cached package.json analysis state rather than starting daemon, registry, or resolver work from the `InlayHintsProvider`. Each dependency entry may show its measured compressed size, `not installed`, `checking...`, `unavailable`, or a deprecation suffix. Each dependency block should also expose a compact measured/total summary when analysis state is available.

**FR-036n** (Medium) - The extension must provide `ImportLens: Compare Imports`, allowing users to compare two package specifiers side by side using the same local daemon sizing path as normal import analysis.

**FR-036o** (Medium) - The extension must provide a static SVG history panel generated from existing bundle impact history data. The webview must keep scripts disabled.

**FR-036p** (Medium) - The extension must support `.importlensignore` using gitignore-style package, path, and import-pattern rules to suppress analysis and decorations for matching imports.

### 5.7 Configuration

**FR-037** (Critical) - The extension must expose the following user-configurable settings via the VS Code settings panel:

| Setting key                  | Type    | Default     | Description                                                                                              |
| ---------------------------- | ------- | ----------- | -------------------------------------------------------------------------------------------------------- |
| `importLens.enabled`         | boolean | `true`      | Toggle the extension on or off                                                                           |
| `importLens.display`         | enum    | `inlayHint` | Display format: `minimal`, `standard`, `verbose`, or `inlayHint`                                         |
| `importLens.inlineRenderer`  | enum    | `colored`   | Inline renderer for `display: "inlayHint"`: `colored` decoration-backed hints or `native` VS Code inlay hints |
| `importLens.compression`     | enum    | `brotli`    | Primary compression format shown in minimal and standard modes. Options: `brotli`, `gzip`, `zstd`, `all` |
| `importLens.debounceMs`      | number  | `300`       | Milliseconds to wait after the last keystroke before sending a request                                   |
| `importLens.showWarnings`    | boolean | `true`      | Show warning indicator for non-tree-shakeable imports                                                    |
| `importLens.useCodeLens`     | boolean | `false`     | Use code lens above the line instead of end-of-line decorations                                          |
| `importLens.enableDiskCache` | boolean | `true`      | Persist computed sizes to disk via redb across editor restarts                                           |
| `importLens.budgets`         | object  | `{}`        | Optional per-import and per-file Brotli thresholds for diagnostics and CLI checks                         |
| `importLens.enableRegistryHints` | boolean | `true`   | Enable short-timeout npm metadata hints cached in globalState and session memory                         |
| `importLens.logLevel`        | enum    | `info`      | Logging verbosity for the ImportLens output channel. Options: `error`, `warn`, `info`, `debug`           |

### 5.8 Daemon Lifecycle

**FR-038** (High) - On extension deactivation (or VS Code window close), the extension host must send a `Shutdown` message over the IPC socket. On receiving this message, the daemon must:
1. Stop accepting new requests.
2. Flush all pending `papaya` entries to `redb`.
3. Close the `redb` database.
4. Remove the Unix socket file (macOS/Linux) or release the named pipe (Windows).
5. Exit the process cleanly within 5 seconds.

If the daemon closes the IPC socket cleanly before the 5-second timeout elapses, the extension host must treat that as a successful exit and skip the escalation sequence below. If the daemon does not exit within 5 seconds of the `Shutdown` message, the extension host must send `SIGTERM` (Unix) or call `TerminateProcess` (Windows) to request termination. If the daemon still has not exited after an additional 2 seconds following the `SIGTERM`, the extension host must send `SIGKILL` (Unix) to forcefully terminate it. (`SIGTERM` can be caught or ignored by the process; `SIGKILL` cannot.) On Windows, `TerminateProcess` is already unconditional and no second step is needed.

### 5.9 Diagnostics and Logging

**FR-039a** (Medium) - When `importLens.useCodeLens` is set to `true`, the extension must register a `CodeLensProvider` for the relevant language selectors and render one `CodeLens` per import line, positioned on the line above the import statement. The lens must display the primary compression size and, when clicked, open the full size breakdown in a hover-style `MarkdownString` notification. The `useCodeLens` setting is independent of `importLens.display`; if `inlayHint` display mode is active simultaneously, inline hint rendering takes precedence and the `CodeLensProvider` must not be registered. The `CodeLens` approach is noted as less space-efficient than inline hints (see D-011) but is retained as an option for users who prefer it.

**FR-039** (High) - When `importLens.display` is set to `inlayHint` and `importLens.inlineRenderer` is `native`, the extension must register an `InlayHintsProvider` with VS Code for the relevant language selectors. The provider must return one `InlayHint` per import line, positioned at the end of the import statement (e.g., after the semicolon), with `kind` set to `undefined` (no `InlayHintKind`) and `paddingLeft` enabled so the hint does not visually run into the code. Import sizes are not parameters or types; using `InlayHintKind.Parameter` or `InlayHintKind.Type` would apply the wrong theme colours (`editorInlayHint.parameterForeground` or `editorInlayHint.typeForeground` respectively). An `undefined` kind falls through to the generic `editorInlayHint.foreground`/`editorInlayHint.background`, which theme authors expect for custom inlay hints. Each `InlayHint` label must be constructed as an array of `InlayHintLabelPart` to allow interactivity, specifically a `command` part that triggers `importLens.showImportDetails` when clicked. It must also set `tooltip` to a `MarkdownString` containing the full size breakdown (raw bytes, minified bytes, all three compressed sizes, `side_effects` status, `is_cjs` indicator, runtime, confidence, and any analysis insights from FR-031b through FR-031f). When a size is unavailable, the tooltip must show a compact unavailable message and a trusted `Copy diagnostics` command link instead of rendering raw daemon logs inline.

**FR-039d** (High) - When `importLens.display` is set to `inlayHint` and `importLens.inlineRenderer` is `colored`, the extension must register a source-range hover provider scoped exclusively to the import specifier string (e.g. `"lodash-es"`) and return the same detailed `MarkdownString` used by native inlay hints and end-of-line decorations. This hover must be tightly scoped to the specifier string so it does not conflict with TypeScript's language-service hover when the user inspects named import bindings. Native inlay mode must continue to rely on the `InlayHint.tooltip` attached to the actual size label.

**FR-039b** (Medium) - The extension must include a note in its README and marketplace description that `importLens.inlineRenderer: "colored"` is the default for confidence-colored inline feedback, while `importLens.inlineRenderer: "native"` is preferred for screen-reader accessibility. End-of-line and decoration-backed inline renderers are not exposed to VS Code's accessibility APIs in the same way as native inlay hints. The native inlay-hint renderer uses the VS Code Inlay Hints API, which is part of the document model and is screen-reader-accessible. The status bar item (FR-033) must always reflect the current operating tier regardless of display mode, as it is accessible to screen readers.

**FR-039c** (High) - The extension output channel must avoid noisy warning duplication. Fallback details for successful low-confidence results belong in diagnostics, hover, report, copied diagnostics, and debug logs. Warning-level output should be reserved for no usable result, daemon/IPC failure, protocol failure, startup failure, or a final unresolved analysis failure, and each request/specifier/error tuple should be logged once.

**FR-040** (High) - The extension must create a VS Code `OutputChannel` named `ImportLens` for structured diagnostic logging. Log messages must include ISO 8601 timestamps and a severity level. The verbosity is controlled by the `importLens.logLevel` setting. The default level must be `info`, and opening the output channel must write a current log-level breadcrumb even if the configured level would otherwise filter normal lifecycle logs.

**FR-041** (High) - The extension must provide a command `ImportLens: Show Logs` that focuses the `ImportLens` output channel in the VS Code panel. This command must be available from the Command Palette at all times, regardless of the extension's current operating tier.

**FR-041a** (High) - The extension must provide a trusted hover command link and registered command `ImportLens: Copy Import Diagnostics`. When invoked from a failed import hover, it must copy the full `ImportResult.error` and `ImportResult.diagnostics` payload for that package to the clipboard. The hover must not display those raw diagnostics directly.

---

## 6. Error Handling and Edge Cases

The system must handle all failure conditions gracefully. No error scenario may produce an uncaught exception in the extension host or a visible error dialog unless explicitly noted below.

| Scenario                                                            | Required Behaviour                                                                                                                                                                                                                                                          |
| ------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Package not installed in node_modules                               | Display a subtle "Package not found" decoration on that import line. Do not send the import to the daemon. Do not display an error dialog.                                                                                                                                  |
| Corrupted, malformed, or versionless `package.json` in node_modules | Fall back to computing a defensively bounded raw directory size of the package folder, excluding nested `node_modules`, VCS directories, and build-cache directories. Mark the result as low confidence, use a leading `~` on the inline size label, and expose fallback details in hover/report/diagnostics. |
| Malformed or incomplete import syntax (user is mid-typing)          | Use OXC parser module-record output when recoverable module information is available. Render partial results if a package name can be identified. Suppress decorations silently if no package name can be resolved.                                                        |
| Daemon crash                                                        | Detect the process exit, wait 1 second, and restart the daemon. Apply exponential backoff on repeated failures (1s, 2s, 4s, 8s, max 30s). After three crashes within 60 seconds, enter degraded mode and display a status bar warning.                                      |
| Socket disconnect without crash                                     | Discard any stale MessagePack payloads currently in the receive buffer. Wait for the next document change event to trigger a fresh request cycle. Do not attempt immediate reconnection to avoid cascading retries on rapid edits.                                          |
| IPC frame larger than 32 MiB                                        | Reject the frame before allocation, close or reset the affected request path, and log a diagnostic. The extension must not attempt to decode the oversized payload.                                                                                                         |
| Batch-like request sent before `HelloMessage`                       | Do not process the request. `BatchRequest` receives per-import protocol errors; `EnumerateExportsRequest` and `FileSizeRequest` receive protocol error responses. Invalidation and prewarm messages are ignored until hello.                                                |
| Unsupported `HelloMessage.version`                                  | Log the unsupported version on the daemon side, close the connection without accepting subsequent requests from that socket, and rely on the extension host startup/connection recovery path.                                                                               |
| Blocking analysis worker panic or join failure                      | Do not panic the Tokio IPC server. Return a protocol diagnostic response for the affected request when the request shape allows it, and keep the daemon process alive for future requests.                                                                                  |
| node_modules folder deleted while extension is running              | The file watcher must detect the deletion. The extension host must send a `CacheInvalidateAll` message (see Section 10.1). The daemon must evict all entries from both `papaya` and `redb`. The extension host must update all affected decorations to "Package not found". |
| redb database corrupted on startup                                  | Log the corruption, delete the corrupted database file, and create a fresh empty database. Continue operation using only the in-memory cache for the current session.                                                                                                       |
| Requested named export missing from a package                       | Return a normal `ImportResult` when partial sizing can continue, include a `missing_export` diagnostic naming the export, and keep the raw diagnostic details in hover-copy output rather than inline UI.                                                                   |
| Namespace import needs conservative fallback                        | Return the best available static size, include an OXC fallback diagnostic, and keep successful imports from the same batch intact.                                                                                                                                          |
| Unsupported native platform or missing daemon binary                | Log the missing runtime and enter degraded mode. Display `ImportLens: Unavailable` in the status bar.                                                                                                                                                                       |
| Daemon binary hash mismatch (NFR-014a)                              | Refuse to spawn the daemon. Log a security warning to the ImportLens output channel at `error` level. Enter degraded mode and display `ImportLens: Unavailable`. Do not show a user-facing error dialog.                                                                    |
| Daemon recycle loop detected (NFR-004b)                             | If more than 5 recycles occurred within any rolling 10-minute window (read from `importlens-recycles.json`), enter degraded mode, log a warning, and display `ImportLens: Unavailable`. Reset counter after a clean 30-minute session with no recycles.                     |
| IPC socket path collision (multiple VS Code windows)                | Each window uses a unique socket path via `VSCODE_PID` or UUID at activation (NFR-014b). If the generated path already exists, generate a fresh UUID and retry once before entering degraded mode.                                                                          |
| Active file is not in a Git repository or Git diff fails            | Skip working-tree delta insights for that analysis cycle. Do not block import sizing, do not show a user-facing error, and do not require Git to be installed for normal size computation.                                                                                  |
| VS Code globalState write fails for history                         | Keep the current import size result visible, log a warning to the ImportLens output channel, and skip only the history/trend update.                                                                                                                                        |
| Named export candidate enumeration fails                            | Keep existing tree-shaking CodeActions available, log the daemon or resolution error, and show a compact warning only for the explicit user-triggered action. Do not rewrite source.                                                                                        |

---

## 7. Non-Functional Requirements

### 7.1 Performance

**NFR-001** (Critical) - The extension must never block the VS Code extension host main thread. All IPC communication, file system reads, and cache lookups must be fully asynchronous.

**NFR-002** (Critical) - Cache hit response time, measured from the moment the debounce fires to the moment decorations are rendered, must be under 50ms on a mid-range developer machine (equivalent to an Apple M2 or Intel Core i7-12th Gen).

**NFR-003** (Critical) - Cache miss computation time for a single named export from a typical npm package (under 500 kB unpacked) must complete within 500ms on the same reference machine.

**NFR-004** (High) - The Rust daemon must consume no more than 100 MB of resident memory during idle operation with the cache populated. During active computation of a batch of 20 imports, peak memory usage must not exceed 400 MB.

**NFR-004a** (High) - The daemon must implement a silent lifecycle recycle to prevent long-term memory fragmentation. Because developers leave VS Code open for days or weeks, even a well-behaved Rust process accumulates allocator fragmentation over time. The daemon must monitor two conditions and gracefully restart itself when either is met: (a) the daemon has been continuously running for more than 4 hours without an active `BatchRequest` in the last 15 minutes. For the purposes of this timer, only `BatchRequest` processing counts as active; `CacheInvalidate` messages, `HelloMessage` handshake, and pre-warm jobs do not reset the idle timer, or (b) the `papaya` in-memory cache exceeds 200,000 entries (approximately 80 to 100 MB at ~500 bytes per entry, consistent with the 100 MB idle memory limit in NFR-004). A graceful restart must: flush all in-memory `papaya` entries to `redb` before exiting, exit cleanly (no signal kill), and rely on the extension host's existing watchdog (FR-015) to respawn it. The restart must be silent to the user; no status bar change or notification must appear unless the restart fails.

**NFR-004b** (High) - The extension host must detect runaway recycle loops, which would never trigger the crash-based degraded mode in FR-015 because graceful recycles exit with code 0. On each daemon respawn, the extension host must read a recycle counter from a small side file at `<globalStoragePath>/importlens-recycles.json`. The daemon must increment this counter and write the current Unix timestamp before beginning its graceful exit. The extension host must enter degraded mode if the counter shows more than 5 recycles within any rolling 10-minute window, log a warning to the ImportLens output channel, and display `ImportLens: Unavailable` in the status bar. The counter file must be reset to zero on a successful session lasting longer than 30 minutes without a recycle.

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

**NFR-011** (Critical) - The daemon must make no outbound network connections. All module resolution must be performed against the local `node_modules` directory only.

**NFR-012** (Critical) - The daemon must operate exclusively via static AST analysis and is prohibited from executing any code found within third-party packages. No subprocess execution, `eval`, dynamic loading, or script interpretation of package contents is permitted under any circumstance.

**NFR-013** (Critical) - The daemon must operate with read-only access limited to `node_modules` packages discovered by walking upward from the active document path. It must not use the first VS Code workspace folder as a hard read boundary, because multi-root windows and nested package workspaces can place the active document in a different dependency tree. The daemon must not write any files other than its own cache database in the VS Code global storage directory.

**NFR-014** (High) - The IPC socket or named pipe must be created with permissions that restrict access to the current user only (mode `0600` on Unix systems). On Unix targets, the daemon must remove the socket file on normal shutdown or lifecycle recycle.

**NFR-014a** (High) - Before spawning the daemon, the extension host must verify the binary's integrity by computing a SHA-256 hash of the daemon executable and comparing it against a known-good hash embedded in the extension package. If the hash does not match, the extension must refuse to spawn the daemon, log a security warning to the `ImportLens` output channel, and enter degraded mode. This prevents execution of tampered binaries.

**NFR-014b** (High) - The IPC socket path (Unix) or named pipe name (Windows) must include a component unique to the VS Code window instance (e.g., the `VSCODE_PID` environment variable or a UUID generated at extension activation) to prevent collisions when multiple VS Code windows are open in different workspaces. Each window must communicate with its own dedicated daemon instance.

### 7.5 Maintainability

**NFR-015** (High) - The Rust daemon crate must be structured so that the compression step, the OXC pipeline step, and the cache layer are each in separate Rust modules with clearly defined interfaces. Adding a new compression format must require changes in a single file only.

**NFR-016** (High) - The TypeScript extension host must be compiled to a single bundled output file using `tsdown`. It must have no runtime npm dependencies other than `oxc-parser` (NAPI binding) and `@msgpack/msgpack`.

**NFR-017** (Medium) - The codebase must achieve at least 70% unit test line coverage on the Rust daemon's core computation logic. Integration tests must cover at least five real-world npm packages: lodash-es, date-fns, zod, react, and uuid. Each package must be pinned to a specific version and its full `node_modules` snapshot must be committed to the repository under `tests/fixtures/packages/<package>@<version>/`. Integration tests must resolve against these local snapshots, not against a live npm registry. This prevents test flakiness caused by upstream package updates that change tree-shaken output.

### 7.6 Extensibility

**NFR-018** (Medium) - Versioned MessagePack request/response schemas must include a `version` field (integer). Protocol v4 is the current native protocol and adds result confidence metadata on top of v3 runtime-aware imports, v2 streaming batch responses, export enumeration, file-level shared sizing, module breakdowns, and per-frame index metadata. The daemon must reject requests with an unrecognised version number and respond with a protocol error response when the request shape allows it. Protocol v1 full-batch `BatchRequest`/`BatchResponse`, v2 request, and v3 request compatibility must be preserved where the missing fields have safe defaults.

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

### 9.1 Extension Host (TypeScript)

| Component     | Technology                                        | Rationale                                                                                                                                                                                                                                     |
| ------------- | ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Language      | TypeScript 6.x (v6.0.3)                           | Bridge release before the Go-based TS 7.0. Uses `module: "esnext"`, `target: "es2025"`, and an explicit `types` array (`["node", "vscode"]`) in `tsconfig.json`. Avoids all legacy patterns deprecated in TS 6 to ease future TS 7 migration. |
| Bundler       | tsdown (Rolldown-based)                           | Produces single-file output, fast builds                                                                                                                                                                                                      |
| Import parser | `oxc-parser` (NAPI)                               | Returns ESM import info directly via module-record arrays without AST walk; uses native NAPI bindings for high performance. The deprecated `@oxc-parser/wasm` must not be used.                              |
| IPC encoding  | `@msgpack/msgpack`                                | Payloads typically 20-40% smaller than JSON; meaningful improvement for batch responses of 20+ imports                                                                                                                                        |
| IPC transport | Unix socket (macOS/Linux) or Named pipe (Windows) | Multiplexed, no stdout pollution                                                                                                                                                                                                              |
| File watching | `vscode.workspace.createFileSystemWatcher`        | Native VS Code API; manages inotify/FSEvents limits safely across all extensions; used to detect package.json changes in node_modules and trigger daemon cache invalidation                                                                   |
| Telemetry     | `vscode.env.createTelemetryLogger` (v1.1 target)  | Anonymised usage telemetry (cache hit rate, tier distribution, recycle frequency). Opt-out respects VS Code global telemetry setting. Instrumentation scaffolding may be added in v1.0 with reporting deferred to v1.1.                       |

### 9.2 Rust Daemon

| Component                  | Crate                        | Rationale                                                                                                                                                                                                                              |
| -------------------------- | ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Module resolution          | `oxc_resolver` (v11.x)       | Production-ready, 30x faster than webpack's enhanced-resolve, used by Rolldown and Nuxt. Note: lives in a separate repository (`oxc-project/oxc-resolver`), versioned independently from the main OXC monorepo.                        |
| Parsing                    | `oxc_parser` (v0.134.x)      | ~3x faster parsing throughput than SWC on JS/TS input, arena-allocated AST, production-ready                                                                                                                                           |
| Semantic analysis          | `oxc_semantic` (v0.134.x)    | Produces scope trees, symbol tables, and binding information for each parsed module. Required for accurate tree-shaking reachability analysis.                                                                                         |
| Tree-shaking               | Custom module graph walker   | Built on `oxc_parser` + `oxc_resolver` + `oxc_semantic`. OXC does NOT provide a standalone tree-shaker; the daemon must implement module graph construction, cross-module reachability analysis, and side-effect tracking. See FR-018. |
| TypeScript / JSX transform | `oxc_transformer` (v0.134.x) | Strips TypeScript types and transforms JSX before minification. Does NOT perform tree-shaking.                                                                                                                                         |
| Minification and mangling  | `oxc_minifier` (v0.134.x)    | Dead code elimination, constant folding, branch pruning, and supported mangling metadata for codegen. Stable 0.x release line; acceptable for size estimation within 1-2% variance.                                                     |
| Code generation            | `oxc_codegen` (v0.134.x)     | Converts the minified AST back to a JavaScript string. Required because `oxc_minifier` operates on the AST, not on text. Supports `minify: true` for whitespace removal.                                                               |
| Gzip compression           | `flate2`                     | Stable, widely used, level 6 default                                                                                                                                                                                                   |
| Brotli compression         | `brotli` crate               | Level 4 balances speed and ratio for real-time use                                                                                                                                                                                     |
| Zstd compression           | `zstd` crate                 | Level 3 provides best speed-to-ratio balance                                                                                                                                                                                           |
| In-memory cache            | `papaya` (v0.2.x)            | Lock-free, deadlock-safe, optimised for read-heavy workloads. Uses a pinning API (`map.pin()`) rather than traditional lock guards.                                                                                                    |
| Persistent cache           | `redb` (v4.x)                | Stable release, pure Rust, ACID, copy-on-write B-trees                                                                                                                                                                                 |
| Concurrency                | `rayon` (v1.12.x)            | Work-stealing thread pool for parallel batch processing. Note: `rayon::join` accepts exactly 2 closures; 3+ parallel tasks require nested `rayon::join` or `rayon::scope`.                                                             |
| IPC serialization          | `rmp-serde` (v1.3.x)         | MessagePack integration with serde, no extra dependencies                                                                                                                                                                              |
| Async runtime              | `tokio`                      | Async socket server for handling concurrent IPC requests                                                                                                                                                                               |

### 9.3 OXC Versioning Note

OXC Rust crates and the `oxc-parser` npm package use 0.x versions, but that does not mean they are alpha quality. OXC follows Rust and npm package versioning before a 1.0 line while publishing production-ready crates and packages. ImportLens pins the OXC analysis stack to one coordinated resolved version across Rust crates and the npm parser binding so parser, AST, semantic, transformer, minifier, and codegen APIs cannot drift independently. OXC upgrades must be performed as an intentional batch with lockfile updates and the dependency policy test suite. The repository must provide `pnpm deps:update:oxc` for targeted stack upgrades, supporting explicit versions, latest resolution, and dry-run mode while updating `daemon/Cargo.toml`, `package.json`, lockfiles, dependency-policy tests, VSIX manifest tests, and this SRS together. The updater must fail before edits when requested versions are invalid or unavailable, OXC monorepo crate versions are not coordinated, or `oxc_mangler` is reintroduced. `oxc_resolver` is versioned independently in a separate repository and is pinned separately. The Docker builder plus `rust-toolchain.toml` follow stable Rust so dependency MSRV bumps are picked up during deliberate upgrade runs. The Docker cross-build toolchain also follows latest stable Zig and latest `cargo-zigbuild` by default, with exact build-arg overrides available only for emergency bisects. Minifier output can differ from SWC by 1 to 2 percent; that variance is acceptable for inline size estimates. See constraint C-001 in Section 13.1.

### 9.4 Dependency Manifest (Current Resolved Versions)

> **This table tracks the current resolved dependency versions and the intended upgrade policy.** OXC monorepo crates and the npm `oxc-parser` binding are pinned as a coordinated stack; `Cargo.lock` and `pnpm-lock.yaml` provide reproducible builds between deliberate upgrade runs. Run `pnpm deps:update:oxc` when intentionally refreshing the OXC stack; use `pnpm deps:update:all` only for broad dependency refreshes. Re-run the dependency policy and `pnpm test:accuracy` suites after either path. OXC versioning policy last audited: **5 June 2026.**

#### 9.4.1 Rust Crates (`Cargo.toml`)

| Crate             | Current Resolved Version | Version Policy | Stability       | Notes                                                                                                                                                                                      |
| ----------------- | ------------------------ | -------------- | --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `oxc_parser`      | 0.134.0                  | exact pin      | ✅ Stable API    | OXC monorepo crate. Must be upgraded in lockstep with the other OXC monorepo crates and npm `oxc-parser`.                                                                                  |
| `oxc_resolver`    | 11.21.0                  | exact pin      | ✅ Stable        | Separate repo from OXC monorepo; versioned independently and upgraded separately.                                                                                                          |
| `oxc_semantic`    | 0.134.0                  | exact pin      | ✅ Stable API    | Must match `oxc_parser` resolved version.                                                                                                                                                  |
| `oxc_transformer` | 0.134.0                  | exact pin      | ✅ Stable API    | TS/JSX stripping only. Does NOT tree-shake.                                                                                                                                                |
| `oxc_minifier`    | 0.134.0                  | exact pin      | ✅ Stable API    | Test every upgrade against the accuracy suite because minified output can shift across releases. The daemon uses the minifier result's scoping and private-member mappings for codegen.     |
| `oxc_codegen`     | 0.134.0                  | exact pin      | ✅ Stable API    | Required for AST -> string. Use `minify: true`.                                                                                                                                            |
| `oxc_allocator`   | 0.134.0                  | exact pin      | ✅ Stable        | Arena allocator. Must match parser resolved version.                                                                                                                                       |
| `oxc_ast`         | 0.134.0                  | exact pin      | ✅ Stable API    | Shared AST data structures. Must match parser resolved version.                                                                                                                            |
| `oxc_ast_visit`   | 0.134.0                  | exact pin      | ✅ Stable API    | AST visitor utilities used for syntax-aware bundle rewrite context such as object shorthand detection. Must match parser resolved version.                                                  |
| `oxc_span`        | 0.134.0                  | exact pin      | ✅ Stable        | Source locations. Must match parser resolved version.                                                                                                                                      |
| `oxc_syntax`      | 0.134.0                  | exact pin      | ✅ Stable API    | Syntax metadata used by the parser and downstream OXC stages. Must match parser resolved version.                                                                                          |
| `papaya`          | 0.2.4            | `~0.2`       | Pre-1.0         | Uses pinning API (`map.pin()`). Recycle triggers at 200,000 cached entries (NFR-004a). Watch for breaking changes.                                                                         |
| `redb`            | 4.1.0            | `^4`         | ✅ Stable (1.0+) | ACID, committed file format. Upgraded from v3 to v4 (April 2026). The redb file format is committed stable; the v3→v4 migration must be handled via cache schema versioning (see FR-026a). |
| `rayon`           | 1.12.0           | `^1.12`      | ✅ Stable        | `join()` takes exactly 2 closures. Use nested calls for 3+.                                                                                                                                |
| `rmp-serde`       | 1.3.1            | `^1.3`       | ✅ Stable        | MessagePack ↔ serde.                                                                                                                                                                       |
| `serde`           | 1.0.228          | `^1`         | ✅ Stable        | With `derive` feature.                                                                                                                                                                     |
| `serde_json`      | 1.0.x            | `^1`         | ✅ Stable        | Structured parsing for `package.json` metadata and small lifecycle files such as `importlens-recycles.json`; avoids ad hoc string parsing.                                                 |
| `tokio`           | 1.52.3           | `^1.52`      | ✅ Stable        | Features: `rt-multi-thread`, `net`, `io-util`, `macros`.                                                                                                                                   |
| `flate2`          | 1.1.9            | `^1.1`       | ✅ Stable        | Gzip level 6.                                                                                                                                                                              |
| `brotli`          | 8.0.3            | `^8`         | ✅ Stable        | Brotli level 4.                                                                                                                                                                            |
| `zstd`            | 0.13.3           | `~0.13`      | ✅ Stable API    | Zstd level 3.                                                                                                                                                                              |
| ~~`num_cpus`~~    | N/A              | N/A          | Removed         | Replaced by `std::thread::available_parallelism()` (stable since Rust 1.59). The `num_cpus` crate is in maintenance mode and provides no value over the stdlib.                            |

#### 9.4.2 npm Packages (`package.json`)

| Package                              | Current Resolved Version | Category                               | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ------------------------------------ | ---------------- | -------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `oxc-parser`                         | 0.134.0          | `dependency`                           | NAPI binding. `package.json` uses an exact `0.134.0` pin so lockfile refreshes cannot accidentally move the extension-side parser away from the Rust-side OXC monorepo crates. Must stay resolved to the same OXC monorepo release as the Rust-side `oxc_parser` crate. **NOT** `@oxc-parser/wasm` (deprecated).                                                                                                                                                                                                                                                               |
| `@oxc-parser/binding-win32-x64-msvc` | 0.134.0          | `dependency` in the Windows VSIX build | Native parser binding required by `oxc-parser` at runtime for the `win32-x64` VSIX. Other platform VSIX builds must include the matching `@oxc-parser/binding-*` package for their target. Because pnpm stores dependencies behind junctions/symlinks and `vsce --no-dependencies` excludes `node_modules`, the VSIX packaging step stages physical copies of `oxc-parser`, `@oxc-project/types`, and the target binding, then invokes `vsce package` with a staged manifest containing no devDependencies.                                                                    |
| `@msgpack/msgpack`                   | 3.1.3            | `dependency`                           | MessagePack encode/decode.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `tsdown`                             | 0.22.1           | `devDependency`                        | Rolldown-based bundler. Output: single-file `extension.js`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `typescript`                         | 6.0.3            | `devDependency`                        | Bridge release to TS 7.0. Type checking only; not a runtime dep. **tsconfig must use**: `module: \"esnext\"`, `target: \"es2025\"`, `types: [\"node\", \"vscode\"]` (explicit), `moduleResolution: \"bundler\"`. Do NOT use TS 5.x.                                                                                                                                                                                                                                                                                                                                            |
| `@types/vscode`                      | 1.90.0           | `devDependency`                        | Intentionally pinned to a baseline VS Code version, not the latest release. The extension's `package.json` must declare `"engines": { "vscode": "^1.90.0" }`. All VS Code APIs used by ImportLens (InlayHintsProvider, FileSystemWatcher, OutputChannel, TelemetryLogger, etc.) are available in 1.90+. VS Code 1.90 was released in May 2024; pinning here guarantees compatibility with almost all popular VS Code forks (Cursor, Windsurf, Antigravity) that often lag upstream. This version is intentionally pinned to match `@types/vscode` and is not the latest; bumping further requires a deliberate decision. |
| `@types/node`                        | 22.15.3          | `devDependency`                        | Explicit Node ambient types for VS Code extension-host APIs used by ImportLens (`fs/promises`, `net`, `child_process`, `crypto`, `path`, and Node's built-in test runner). This keeps `types` explicit instead of relying on TypeScript auto-inclusion.                                                                                                                                                                                                                                                                                                                        |
| `@vscode/vsce`                       | 3.9.1            | `devDependency`                        | VSIX packaging and publishing. Use `--no-dependencies` flag when building VSIX for bundled extensions.                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |

#### 9.4.3 Build Tools

| Tool                  | Version                  | Purpose                  | Notes                                                                                                                                                                                                                                                                                                     |
| --------------------- | ------------------------ | ------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust toolchain        | stable (Edition 2024)   | Daemon compilation       | The daemon does not declare a fixed MSRV while ImportLens intentionally tracks latest package versions. `rust-toolchain.toml` and `Dockerfile.build` use stable Rust so OXC or other dependency MSRV bumps are picked up with normal toolchain updates.                                                     |
| Zig                   | latest stable           | Cross-target linker      | Used by the Docker `--zigbuild` packaging path for Linux and macOS binaries. It is not needed for the native Windows package path. `Dockerfile.build` resolves `ZIG_VERSION=latest` from Zig's official download index and still permits an exact `ZIG_VERSION` build arg for bisecting upstream regressions. |
| `cargo-zigbuild`      | latest                  | Rust cross-compilation   | Wraps Cargo so Rust builds can use Zig's C toolchain/linker for Linux and macOS targets from the Linux Docker builder. The default Docker build installs the latest crate; an exact `CARGO_ZIGBUILD_VERSION` build arg may be used only to isolate a broken upstream release.                               |
| `wasm-opt` (Binaryen) | 123                      | WASM binary optimization | Run with `-Oz` after `cargo build --target wasm32-wasip1-threads`.                                                                                                                                                                                                                                        |
| Node.js               | >= 20 LTS                | Extension development    | Required by VS Code extension host.                                                                                                                                                                                                                                                                       |

#### 9.4.4 Deprecated / Banned Packages

> **These packages must NOT be used anywhere in the project.** Any appearance in `Cargo.toml`, `package.json`, or source code is a build error.

| Package                  | Reason                                                        | Replacement                                                     |
| ------------------------ | ------------------------------------------------------------- | --------------------------------------------------------------- |
| `@oxc-parser/wasm` (npm) | Officially deprecated. No longer maintained.                  | `oxc-parser` (NAPI, exact OXC monorepo pin)                     |
| `sled` (Rust)            | Never shipped 1.0. Unstable on-disk format.                   | `redb` (v4.x, stable format)                                    |
| `dashmap` (Rust)         | Deadlock risk with sharded RwLock under read-heavy workloads. | `papaya` (v0.2.x, lock-free)                                    |
| `num_cpus` (Rust)        | Maintenance mode since June 2023. Superseded by stdlib.       | `std::thread::available_parallelism()` (stable since Rust 1.59) |

---

## 10. Component Specifications

### 10.1 IPC Message Schemas

#### BatchRequest

```typescript
interface BatchRequest {
  version: number;              // Protocol version, currently 4
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
  side_effects: boolean;          // true if sideEffects field is absent, true, or a matching array entry
  truly_treeshakeable: boolean;   // false if named export size is within 5% of full package size
                                  // or sideEffects metadata forces conservative full-graph analysis
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

#### HelloMessage

Sent by the extension host immediately after opening the socket connection. The daemon must validate the hello protocol version before accepting the handshake and must not process any request until a valid `HelloMessage` has been received.

```typescript
interface HelloMessage {
  type: "hello";
  version: number;              // Protocol version, currently 4
  workspace_root: string;       // Absolute path to the active analysis root.
  storage_path: string;         // Absolute VS Code globalStoragePath for cache and lifecycle files
  enable_disk_cache: boolean;   // From importLens.enableDiskCache setting
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

#### ShutdownMessage

Sent by the extension host on extension deactivation. The daemon must flush caches and exit. See FR-038.

```typescript
interface ShutdownMessage {
  type: "shutdown";
}
```

### 10.2 Cache Key Format

The cache key for both `papaya` and `redb` size entries is a UTF-8 string using the v3 prefix and a hex-encoded MessagePack `CacheIdentityV3` payload:

```
v3:<hex-msgpack-cache-identity>
```

The identity payload contains `analyzer_version`, `specifier`, root `package_name`, `package_version`, optional canonical `package_root`, optional canonical `entry_path`, `runtime`, `import_kind`, sorted/deduplicated `named_exports`, and manifest/entry fingerprints when available. Sorting named exports ensures import order does not create duplicate entries. Namespace, default, and dynamic imports are distinguished by `import_kind`, so a named export literally called `"dynamic"` cannot collide with dynamic-import analysis.

The `specifier` field in `ImportRequest` must carry the full subpath (e.g. `"date-fns/format"`) so the daemon can resolve the correct entry point via `oxc_resolver`. The `package` field carries the root package name only (e.g. `"date-fns"`) for `node_modules` lookup purposes. The `version` field is read from the root package's `package.json` regardless of subpath, since subpaths do not have independent versions.

### 10.3 Virtual Entry Module

For each cache miss, the daemon constructs an in-memory virtual file. The pattern varies by import kind:

```javascript
// Named imports
export { debounce, throttle } from 'lodash-es';

// Default import
export { default } from 'react';

// Namespace import
export * from 'lodash-es';

// Dynamic import: the package entry point is resolved directly
// and passed to the OXC pipeline without a virtual entry file
```

Re-exports are semantically unambiguous to tree-shakers. The bundler cannot drop a named export from an entry module regardless of how aggressive dead code elimination is.

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
    ├─ Locate native binary in extension/bin/<platform>/import-lens-daemon
    │       |
    │       ├─ Found → spawn process, pipe stdout/stderr, open socket, send HelloMessage handshake
    │       │
    │       └─ Not found → enter degraded mode, show status bar warning
    |
    Daemon starts
        |
        ├─ Read <globalStoragePath>/importlens-recycles.json (NFR-004b)
        │   └─ If recycle rate exceeds threshold: enter degraded mode immediately
        ├─ Open redb database at <globalStoragePath>/importlens.redb
        │   └─ If corrupted: delete, create fresh, log warning
        ├─ Preload at most the configured recent valid entries into papaya
        ├─ Serve other disk entries lazily and promote them into memory on hit
        ├─ Begin listening on socket / named pipe
        └─ After hello, prewarm up to 20 recent valid cache entries
```

### 10.6 Tree-Shakeability Detection

After computing the size of the requested named exports, the daemon computes the full-package variant. If:

```
named_export_size / full_package_size >= 0.95
```

then `truly_treeshakeable` is set to `false`. This catches packages that declare `"sideEffects": false` in `package.json` but whose internal module graph does not actually support granular export isolation. The flag is also `false` when `sideEffects` is absent or `true` because the daemon must include the full parsed graph conservatively. For `sideEffects` arrays, the flag is false only when the resolved entry or included graph path matches a side-effect pattern; non-matching array entries may be tree-shaken normally.

### 10.7 Module Graph Walk Algorithm

This section specifies the algorithm that `graph.rs` and `reachability.rs` must implement. It exists to resolve ambiguities that FR-018 leaves open at the implementation level.

**Data structures**

```
ModuleGraph {
  modules: HashMap<AbsolutePath, Module>,
  entry: AbsolutePath,
  dependency_paths: Vec<AbsolutePath>,
  diagnostics: Vec<GraphDiagnostic>,
}

Module {
  path: AbsolutePath,
  ast: OxcAst,              // produced by oxc_parser
  semantic: OxcSemantic,    // produced by oxc_semantic
  imports: Vec<ModuleEdge>, // resolved import statements
  external_imports: Vec<ExternalImportEdge>,
  exports: Vec<ExportDef>,  // named, default, re-export
  transformed_src: String,  // output of oxc_transformer (TS stripped, JSX lowered)
}

ModuleEdge {
  specifier: String,        // raw specifier as written in source
  resolved: AbsolutePath,   // result of oxc_resolver
  kind: ImportKind,         // Static | Dynamic
}

ExternalImportEdge {
  specifier: String,        // raw specifier for builtin, peer, or unresolved external
  imported_name: String,
  local_name: String,
}
```

**Graph construction (graph.rs)**

```
fn build_graph(entry_path, resolver) -> ModuleGraph:
  graph = ModuleGraph::new()
  queue = [entry_path]
  visited = HashSet::new()
  active_stack = HashSet::new()
  total_source_bytes = 0

  while queue is not empty:
    path = queue.pop()
    if path in visited: continue      // handles circular dependencies
    if graph.module_count == 2000: fail("module count limit exceeded")
    visited.insert(path)
    active_stack.insert(path)

    source = fs::read(path)
    if source.byte_length > 5 MiB: fail("module source size limit exceeded")
    total_source_bytes += source.byte_length
    if total_source_bytes > 50 MiB: fail("graph source size limit exceeded")
    ast = oxc_parser::parse(source)
    semantic = oxc_semantic::build(ast)
    imports = collect_static_imports(ast)

    resolved_edges = []
    for import in imports:
      match resolver.resolve(import.specifier, from = path):
        Ok(resolved_path) =>
          resolved_edges.push(ModuleEdge { specifier, resolved: resolved_path })
          if resolved_path in active_stack:
            graph.diagnostics.push(circular_dependency(path, resolved_path))
          else if resolved_path not in visited:
            queue.push(resolved_path)
        Err(e) =>
          // Treat Node builtins, unresolved peers, and unsupported externals as external
          // and keep a structured diagnostic instead of failing the whole import.
          graph.diagnostics.push(external_resolution(import.specifier, path, e))

    graph.insert(path, Module { path, ast, semantic, imports: resolved_edges, ... })
    active_stack.remove(path)

  return graph
```

The visited-set check on every dequeue prevents infinite loops on circular dependencies. A module that is visited twice (A imports B imports A) will have its edges walked once; the back-edge is recorded as a `circular_dependency` diagnostic and the second encounter is a no-op. Shared diamond dependencies must not be reported as cycles.

**Reachability walk (reachability.rs)**

```
fn mark_reachable(graph, entry_exports) -> HashSet<(AbsolutePath, SymbolId)>:
  reachable = HashSet::new()
  worklist = entry_exports.map(|e| (graph.entry, e.symbol_id))

  while worklist is not empty:
    (module_path, symbol_id) = worklist.pop()
    key = (module_path, symbol_id)
    if key in reachable: continue
    reachable.insert(key)

    // follow re-exports and bindings through the semantic graph
    for binding in graph[module_path].semantic.references_of(symbol_id):
      match binding:
        ReExport { from_module, from_symbol } =>
          worklist.push((resolved_path_of(from_module), from_symbol))
        ImportBinding { from_module, from_symbol } =>
          worklist.push((resolved_path_of(from_module), from_symbol))
        LocalBinding { .. } =>
          // symbol is defined locally; no further traversal needed

  return reachable
```

**Scope renaming before concatenation**

Before concatenating module sources, each module's local bindings must be renamed to a module-unique prefix to prevent collisions. The prefix is derived from the module's index in topological order: `__m{N}_{originalName}`. Renaming is applied to source slices using `oxc_semantic` binding and reference spans. The renamer must preserve UTF-8 boundaries, object shorthand, object destructuring, array destructuring, and rest binding semantics.

**Side-effect handling**

If `sideEffects: false` is set in the package's `package.json`, modules that contribute no reachable symbols are excluded from concatenation entirely. If `sideEffects` is absent or `true`, all parsed modules are included in concatenation regardless of reachability, and only the minification step removes dead code. If `sideEffects` is an array, the daemon evaluates the resolved entry and included module paths against the configured patterns: matching modules force conservative inclusion and side-effect diagnostics, while non-matching paths may still be pruned through normal reachability.

**Graph cache**

The daemon may keep parsed module graphs in a side in-memory cache keyed by canonical entry path and resolver/runtime profile. This cache is an optimization only: size results remain keyed by the structured v3 cache identity and persisted in `redb`; graph cache misses must not change user-visible results.

---

## 11. Data Models

### 11.1 Persistent Cache Schema (redb)

The `redb` database schema version is `3` and contains these tables:

| Table name      | Key type                                      | Value type                                       |
| --------------- | --------------------------------------------- | ------------------------------------------------ |
| `metadata`      | `&str`                                        | `u64` (`schema_version`)                         |
| `size_cache`    | `&str` (cache key as defined in Section 10.2) | `&[u8]` (MessagePack-encoded cache envelope)     |
| `cache_recents` | `&str` (cache key as defined in Section 10.2) | `u64` (last-used Unix timestamp in milliseconds) |

`size_cache` values persist an internal cache envelope containing the public `ImportResult`, analyzer version, package identity, dependency fingerprints, and full contribution list needed for accurate shared-byte accounting. The daemon normalizes `cache_hit` to `false` before writing and sets it to `true` when serving a memory or disk hit. `cache_recents` is updated on insert and through batched/debounced hit touches, and is used to select up to 20 recent entries for bounded startup preload and best-effort prewarm after hello.

### 11.2 Extension Global Storage

The extension stores lightweight UI history in VS Code `globalState`. These records are separate from the daemon's `redb` cache and are not used for daemon cache identity or correctness.

| Key                              | Value shape                 | Purpose                                                                                  |
| -------------------------------- | --------------------------- | ---------------------------------------------------------------------------------------- |
| `importLens.bundleImpactHistory` | `BundleImpactHistoryItem[]` | Recent current-file total measurements shown by `ImportLens: Show Bundle Impact History` |
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

| Component                                        | Uncompressed               | In VSIX (compressed)     |
| ------------------------------------------------ | -------------------------- | ------------------------ |
| Native Rust daemon (OXC pipeline, stripped, LTO) | ~12-15 MB                  | ~9-11 MB                 |
| `oxc-parser` NAPI binary                         | ~2 MB                      | ~1 MB                    |
| `@msgpack/msgpack`                               | ~200 kB                    | ~80 kB                   |
| Extension TypeScript bundle (tsdown output)      | ~800 kB                    | ~350 kB                  |
| Metadata, icons, manifests                       | ~50 kB                     | ~20 kB                   |
| **Total per-platform VSIX**                      | **~15-18 MB uncompressed** | **~10-13 MB compressed** |

All platform targets fall within the 20 MB hard limit defined in NFR-007. The switch from `@oxc-parser/wasm` (~9 MB) to `oxc-parser` NAPI (~2 MB) provides significant headroom.

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
- When pnpm is used, each VSIX build must stage physical copies of the bundled extension, target daemon binary, `oxc-parser`, `@oxc-project/types`, and the target `@oxc-parser/binding-*` package, then invoke `@vscode/vsce package --target <platform>` from the staging directory. This avoids publishing pnpm junctions and allows `vsce` to include the native parser runtime via its production dependency walker.
- The CI pipeline must measure the size of each output VSIX and fail the publish step if any target exceeds 20 MB (enforcing AC-001 and NFR-007).
- Each platform VSIX must be built and published in the same CI run to ensure version consistency across all targets.
- The integration test suite and all five acceptance criteria must pass before any VSIX is published.

---

## 13. Constraints and Assumptions

### 13.1 Technical Constraints

**C-001:** OXC Rust crates and the `oxc-parser` npm package use 0.x versions, but those versions are not alpha releases. ImportLens pins OXC monorepo crates, `oxc-parser`, and `oxc_resolver` to exact versions because parser/minifier/resolver behavior directly affects size accuracy. Dependency refreshes must therefore be explicit coordinated changes with focused parser, graph, minifier, and packaging verification. Size estimation accuracy of approximately plus or minus 2 percent remains acceptable for an inline hint tool. **Fallback strategy:** If `oxc_minifier` exhibits correctness regressions in the integration test suite after an upgrade, the team must pin to the last known-good version and file an upstream issue. No release VSIX will ship with a minifier version that fails the integration suite. As a last resort, the daemon may skip minification entirely and report only raw + compressed sizes, with a `(no-minify)` suffix on decorations.

**C-002:** The `oxc-parser` npm package (NAPI binding, current resolved baseline v0.134.0) uses native bindings and does NOT work in browser environments. The deprecated `@oxc-parser/wasm` package must not be used due to its deprecated status. For VS Code for the Web, the extension enters degraded mode with no parsing capability. The Rust-side `oxc_parser` crate and npm `oxc-parser` package are released from the same OXC monorepo on the same cadence; after every dependency refresh, both lockfiles must resolve to the same OXC monorepo version.

**C-003:** Rolldown's Rust embedding API (`rolldown_core` on crates.io) does not yet expose a stable public interface for programmatic use as a Rust library. Rolldown is therefore not used directly in this project. A custom module graph walker is implemented instead using OXC primitives. This constraint must be re-evaluated when Rolldown's Rust API stabilises. See Appendix C: Technology Watch.

**C-004:** A WASM daemon fallback is deferred to v1.1 or later. The candidate target is `wasm32-wasip1-threads`, which is an experimental Rust/LLVM target. Thread support requires `SharedArrayBuffer` and cross-origin isolation (`Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp`). Any future WASM binary must be compiled with an explicit `--max-memory` linker flag set to at least `67108864` (64 MB) to provide sufficient headroom for Rayon's thread stacks; larger values may be needed if the module graph walker exceeds this during deep dependency trees. VS Code for the Web remains degraded mode in v1.0 because browser `SharedArrayBuffer` availability and local `node_modules` access are not guaranteed. The `wasi-threads` proposal used by this target is considered legacy; the industry is transitioning toward the Component Model. See Appendix C: Technology Watch.

### 13.2 Out-of-Scope Decisions

- **napi-rs native addon:** Rejected because a panic in a native addon crashes the entire VS Code extension host. See Section 4.5.
- **SWC minifier:** Rejected because its binary adds approximately 25 to 27 MB per target, violating NFR-007. See Section 4.2.
- **JSON over IPC:** Rejected in favour of MessagePack for performance reasons. See Section 4.4.
- **ESBuild:** Rejected because it is written in Go and requires managing a separate WASM execution layer from Rust. See Section 4.1.
- **`@oxc-parser/wasm`:** Deprecated npm package. Replaced by `oxc-parser` (NAPI). See Section 4.3.

### 13.3 Assumptions

- Users have npm, yarn, or pnpm with hoisting installed and have run a package install command. The extension does not install packages itself.
- The VS Code global storage path is writable. If it is not, the persistent cache is skipped gracefully and all results are held in memory for the duration of the session.
- Packages shipping only CommonJS are analyzed statically where possible. Literal relative `require()` graphs and common export forms produce better approximations, but dynamic or unsupported CJS still falls back conservatively. The extension will display a `CJS` indicator next to the size for these packages.
- The extension assumes `node_modules` is fully installed. It will not trigger or assist with package installation.
- The user's environment is VS Code Desktop for full functionality. VS Code for the Web provides degraded mode only.

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
│   │   ├── watcherActions.ts          # node_modules invalidation batching and refresh hooks
│   │   ├── analysis/
│   │   │   ├── fileSize.ts            # current-file size summary formatting
│   │   │   ├── freshness.ts           # request freshness tracking
│   │   │   ├── gitDiff.ts             # working-tree changed-line extraction
│   │   │   ├── history.ts             # bundle and per-import history globalState helpers
│   │   │   ├── insights.ts            # extension-side analysis insight builder
│   │   │   ├── request.ts             # DetectedImport to BatchRequest item mapping
│   │   │   ├── status.ts              # loading/unavailable state helpers
│   │   │   └── state.ts               # Per-document import analysis state
│   │   ├── imports/
│   │   │   ├── parser.ts              # oxc-parser (NAPI) import extraction via parseSync()
│   │   │   ├── scriptRegions.ts       # Svelte/Astro/Vue script region extraction and runtime labeling
│   │   │   ├── resolver.ts            # package.json version resolution (handles scoped packages)
│   │   │   ├── specifier.ts           # package/builtin/relative specifier helpers
│   │   │   ├── positions.ts           # offset-to-position mapping helpers
│   │   │   └── types.ts               # import detection data models
│   │   ├── guidance/
│   │   │   ├── packageJsonAnalysis.ts # cached package.json dependency analysis controller
│   │   │   ├── packageJsonDependencies.ts # JSON-aware dependency block/range extraction
│   │   │   ├── packageJsonState.ts    # package.json dependency analysis state types
│   │   │   └── registryHints.ts       # short-timeout npm metadata hints
│   │   ├── ipc/
│   │   │   ├── client.ts              # Socket/pipe connection management
│   │   │   ├── protocol.ts            # Protocol v4 IPC types
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
│   │   ├── report/
│   │   │   ├── reportModel.ts         # Workspace report rows, confidence summaries, module breakdown
│   │   │   └── workspaceScanner.ts    # Workspace import scanner for report command
│   │   ├── watcher.ts                 # vscode.workspace.createFileSystemWatcher; sends CacheInvalidate IPC messages
│   │   ├── ui/
│   │   │   ├── currentFileSize.ts     # current-file total and bundle impact history commands
│   │   │   ├── decorations.ts         # End-of-line text decorations
│   │   │   ├── inlayHints.ts          # InlayHintsProvider for inlayHint display mode
│   │   │   ├── codelens.ts            # Code lens provider
│   │   │   ├── completions.ts         # Named import member completion provider
│   │   │   ├── displayGuards.ts       # display-mode enablement helpers
│   │   │   ├── format.ts              # size and display label formatting
│   │   │   ├── packageJsonInlayHints.ts # package.json dependency inlay hint provider
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
│       ├── ipc/
│       │   ├── mod.rs
│       │   ├── codec.rs               # MessagePack length-prefix codec
│       │   ├── server.rs              # Unix socket / named pipe listener
│       │   └── protocol.rs            # Protocol v4 serde types
│       ├── pipeline/
│       │   ├── mod.rs
│       │   ├── resolver.rs            # oxc_resolver usage
│       │   ├── graph.rs               # Module graph walker (oxc_parser + oxc_resolver + oxc_semantic)
│       │   ├── reachability.rs        # Reachability analysis and dead code marking
│       │   ├── bundle.rs              # UTF-8-safe module concatenation and renaming
│       │   ├── cjs.rs                 # Static CommonJS graph analysis
│       │   ├── file_size.rs           # File-level shared import cost computation
│       │   ├── minify.rs              # oxc_minifier + oxc_codegen usage
│       │   └── compress.rs            # flate2 + brotli + zstd (nested rayon::join)
│       ├── cache/
│       │   ├── mod.rs
│       │   ├── key.rs                 # Cache key formatting
│       │   ├── memory.rs              # papaya HashMap (pinning API)
│       │   └── disk.rs                # redb read/write
│       ├── lifecycle.rs                # Graceful shutdown, self-recycle (NFR-004a), recycle counter write (NFR-004b)
│       └── prefetch.rs                # Background pre-warm logic
│
├── bin/                               # Native daemon binaries (gitignored, CI-populated)
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

| ID    | Decision                                                                                  | Rationale                                                                                                                                                                                                                                                                                                                                                    | Alternatives Considered                                                                                                                                                                         |
| ----- | ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| D-001 | Separate daemon process over napi-rs native addon                                         | A panic in a native addon crashes the VS Code extension host. A separate process isolates failures completely.                                                                                                                                                                                                                                               | napi-rs native addon (rejected: crash risk to editor)                                                                                                                                           |
| D-002 | OXC for the full pipeline (parse, resolve, semantic, tree-shake, minify, mangle, codegen) | Single AST representation shared across all stages eliminates re-parsing overhead. All OXC crates are embeddable in Rust. OXC is used internally by Rolldown and Vite 8. Note: OXC does not provide a standalone tree-shaker; a custom module graph walker is required.                                                                                      | Rolldown Rust API (rejected: no stable embedding API); ESBuild (rejected: written in Go, requires separate WASM layer from Rust)                                                                |
| D-003 | oxc_minifier over swc_core                                                                | SWC platform binaries are approximately 25 to 27 MB per target, violating the 20 MB VSIX limit. For size estimation, 1-2% accuracy variance is acceptable.                                                                                                                                                                                                   | swc_core (rejected: distribution size); Terser (rejected: requires Node.js subprocess)                                                                                                          |
| D-004 | MessagePack over JSON for IPC                                                             | Payloads typically 20-40% smaller than JSON. In the Rust rmp-serde path, deserialization is consistently faster. Meaningful for batch responses of 20+ imports.                                                                                                                                                                                              | JSON (rejected: performance); Protocol Buffers (rejected: schema overhead disproportionate for this local IPC protocol)                                                                         |
| D-005 | `oxc-parser` (NAPI) over TypeScript Compiler API                                          | Returns ESM import info directly via `result.module.staticImports` without AST traversal. No `typescript` package dependency. Returns recoverable diagnostic details for malformed code while preserving high-performance native parsing for valid documents. The deprecated `@oxc-parser/wasm` package is not used.                                      | TypeScript Compiler API (rejected: heavy, not WASM-compatible); `@oxc-parser/wasm` (rejected: deprecated); Regex (rejected: fails on multi-line and complex syntax)                             |
| D-006 | papaya over DashMap for in-memory cache                                                   | papaya is lock-free and deadlock-safe. DashMap uses sharded RwLock which can deadlock when holding references. The import size workload is read-heavy after initial warmup.                                                                                                                                                                                  | DashMap (rejected: locking semantics risk for read-heavy pattern)                                                                                                                               |
| D-007 | redb over sled for persistent cache                                                       | redb hit 1.0 stable with a committed stable file format. sled has never shipped 1.0 and its on-disk format remains unstable.                                                                                                                                                                                                                                 | sled (rejected: not stable); rusqlite/SQLite (viable but adds a C FFI dependency)                                                                                                               |
| D-008 | Three compression formats (gzip, brotli, zstd)                                            | All three are in common production use as of 2026. CDNs serve all three. Running them in parallel with nested rayon::join adds negligible latency.                                                                                                                                                                                                           | Gzip only (rejected: brotli and zstd offer meaningfully better ratios); Brotli only (rejected: zstd is now mainstream)                                                                          |
| D-009 | Platform-specific VSIX distribution                                                       | Users download only the binary for their own platform. Each VSIX is 10-13 MB rather than a single 120+ MB universal package.                                                                                                                                                                                                                                 | Universal VSIX (rejected: unacceptable total size); Runtime download of daemon binary (rejected: requires network at activation)                                                                |
| D-010 | Custom module graph walker over Rolldown embedding                                        | Rolldown does not expose a stable Rust API (C-003). Building a custom walker from `oxc_parser` + `oxc_resolver` + `oxc_semantic` provides full control over reachability analysis and side-effect tracking.                                                                                                                                                  | Rolldown Rust API (rejected: unstable); Skip tree-shaking (rejected: inaccurate sizes for named imports)                                                                                        |
| D-011 | Hybrid inline rendering                                                                   | VS Code native inlay hints are accessible, provide reliable size-label hovers, and integrate with editor controls, but the API cannot assign arbitrary colors per hint. ImportLens therefore defaults to decoration-backed colored inline hints through `importLens.inlineRenderer: "colored"` for confidence visibility, while keeping native inlay hints available for users who prioritize screen-reader accessibility.                                                                                              | Native InlayHints only (rejected: no per-hint confidence colors); colored decorations only (rejected: weaker accessibility); end-of-line decorations only (rejected: less inline and less accessible); CodeLens only (rejected: takes full line)            |
| D-012 | TypeScript 6.x over TypeScript 5.x                                                        | TS 6.0 is the current stable release (March 2026). It modernizes tsconfig defaults, requires explicit ambient type inclusion (`types: ["node", "vscode"]` for this extension), deprecates legacy patterns, and serves as the migration bridge to the native Go-based TS 7.0. Starting on TS 6 now avoids a painful double-migration later.                   | TypeScript 5.x (rejected: legacy defaults, will require migration to 6.x before 7.x anyway)                                                                                                     |
| D-013 | `request_id` field in BatchRequest/BatchResponse for cancellation                         | Timing-based heuristics for discarding stale responses are fragile when two requests are fired within milliseconds of each other. An explicit monotonic ID makes the discard decision unambiguous at zero protocol cost.                                                                                                                                     | Timing-only approach (rejected: race condition on fast edits); sequence number on daemon side only (rejected: daemon has no state to track which request is current)                            |
| D-014 | `CacheInvalidateAll` as a distinct message type                                           | Sending one `CacheInvalidate` per package when `node_modules` is deleted would produce hundreds of IPC messages in a large project. A single bulk message is more efficient and avoids buffer pressure on the socket. The 20-package threshold is a pragmatic cutoff; below it, per-package messages give the daemon more granular invalidation information. | Always use bulk (rejected: loses granularity for small changes); always use per-package (rejected: floods socket on full reinstall)                                                             |
| D-015 | Extension-side insight enrichment over daemon protocol expansion                          | Git diff state, VS Code globalState history, and UI-only barrel warnings are editor-context features. Keeping them in the extension avoids changing the native protocol for data the daemon cannot independently know and keeps daemon cache identity stable.                                                                                                | Add fields to `ImportResult` for every insight (rejected: daemon lacks editor/Git context); compute all insights in the daemon (rejected: would require Git and VS Code storage access in Rust) |
| D-016 | Clipboard named-import candidates over automatic namespace rewrites                       | Rewriting `import * as ns` safely requires semantic usage rewriting across the file, including property accesses and potential shadowing. The v1 feature enumerates exports and copies a candidate import while leaving code changes under user control.                                                                                                     | Automatic rewrite CodeAction (rejected: unsafe without full semantic transform); no action (rejected: misses a high-value tree-shaking improvement path)                                        |

---

## 16. Appendix C: Technology Watch

This table tracks components that are currently used with known limitations, or where a better alternative exists but is not yet stable enough for production use. Each item should be re-evaluated at the specified cadence.

| Component                                 | Current State                                                                                                                                     | Watch For                                                                                                                                          | Impact on ImportLens                                                                                                                                                                                                              | Re-evaluate           |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------- |
| `oxc_minifier`                            | Stable 0.x release line, currently resolved to 0.134.0. Produces 1-2% variance from SWC.                                                          | New OXC releases; minifier API or output changes.                                                                                                  | Upgrade OXC crates as a coordinated batch; re-run integration suite to confirm no accuracy regressions.                                                                                                                           | Every OXC release     |
| `oxc_resolver`                            | Currently resolved to 11.21.0. Separate repository (`oxc-project/oxc-resolver`), versioned independently from the OXC monorepo. Currently on major version 11. | Major version bump (e.g. 12.x); breaking changes to `ResolverOptions` or the `resolve()` API.                                                      | May require `Cargo.toml` update and code changes in `resolver.rs`. Upgrade separately from the OXC monorepo batch and run integration suite before merging.                                                                       | Each release          |
| Rolldown Rust API (`rolldown_core`)       | No stable public API. ImportLens uses a custom module graph walker instead.                                                                       | Stable embeddable Rust crate on crates.io with tree-shaking API.                                                                                   | Would replace the custom module graph and reachability code (`graph.rs` + `reachability.rs`), significantly reducing code and improving accuracy. This is the single highest-impact migration.                                    | Quarterly             |
| `wasm32-wasip1-threads`                   | Experimental Rust/LLVM target. Deferred v1.1 candidate; not a v1.0 runtime path.                                                                  | WASI Preview 2 / Component Model threading (`wasm32-wasip2`). The `wasi-threads` proposal is legacy; `shared-everything-threads` is the successor. | May require retargeting before a future WASM fallback ships.                                                                                                                                                                      | Semi-annually         |
| `@vscode/wasm-wasi-core`                  | Supports WASI Preview 1 with experimental thread support. Deferred v1.1 candidate dependency.                                                     | WASI Preview 2 support, Component Model integration, improved `SharedArrayBuffer` ergonomics.                                                      | Better thread reliability and broader environment support, subject to VS Code Desktop and Web limitations.                                                                                                                        | Semi-annually         |
| `oxc-parser` (npm, NAPI)                  | v0.134. Active, replaces deprecated `@oxc-parser/wasm`. No WASM/browser support.                                                                  | Potential official WASM sub-export (e.g. `oxc-parser/wasm`) or Component Model-based distribution.                                                 | Would restore parsing capability in VS Code for the Web, upgrading it from degraded mode in a future release.                                                                                                                     | Quarterly             |
| `papaya`                                  | v0.2.4. Pre-1.0 but actively maintained. Uses seize-based GC.                                                                                     | 1.0 stable release; API changes to pinning semantics.                                                                                              | Minor migration effort if pinning API changes. Lock-free design is correct for the workload.                                                                                                                                      | Semi-annually         |
| VS Code Inlay Hints API                   | Stable. Used as an optional display mode.                                                                                                         | Enhanced styling support (colors, icons), positioning improvements.                                                                                | Richer size display within inlay hints. Currently limited to plain text.                                                                                                                                                          | With VS Code releases |
| `redb`                                    | v4.x stable. ACID, pure Rust.                                                                                                                     | Major version bumps; potential API changes.                                                                                                        | Migration effort proportional to API surface changes. File format is committed stable. Cache schema versioning (FR-026a) ensures seamless upgrades.                                                                               | Annually              |
| TypeScript 7.0 ("Corsa")                  | Not yet released. Native Go-based compiler rewrite by Microsoft. TS 6.x is the bridge release.                                                    | Stable release on npm. Expected to provide 10x+ type-checking speedup.                                                                             | Requires `tsconfig.json` to already use TS 6 modern defaults (which ImportLens does). Migration should be straightforward: update `devDependency`, run `tsc --noEmit`, fix any new diagnostics. No runtime code changes expected. | On release            |
| VS Code engine version (`engines.vscode`) | Currently `^1.90.0`. All required APIs (InlayHintsProvider, FileSystemWatcher, TelemetryLogger, etc.) available at this version.                 | New stable APIs that would benefit ImportLens: richer decoration API, improved CodeLens rendering, enhanced inlay hint styling.                    | Raise `engines.vscode` and `@types/vscode` in tandem. Any bump excludes users on older VS Code versions and third-party forks; evaluate installed-base data before bumping.                                                                             | Annually              |
