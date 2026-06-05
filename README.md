# ImportLens

ImportLens is a blazingly fast Visual Studio Code extension that displays the real-world post-bundle cost of your npm imports directly inline as you type.

Unlike existing import cost calculators that spin up heavy Node.js bundlers, ImportLens offloads all computation to a highly optimized background Rust daemon powered by the **OXC** (Oxidation Compiler) toolchain. It performs real tree-shaking, minification, and compression in milliseconds without blocking your editor or consuming massive amounts of memory.

## Features

- ⚡ **Instant Feedback:** See post-tree-shake, minified, and compressed (Gzip, Brotli, Zstd) sizes inline.
- 🌳 **Real Tree-shaking:** Calculates sizes for named, default, namespace, dynamic imports, re-exports, and named export candidates.
- 🦀 **Rust Daemon Engine:** Built on `oxc_parser`, `oxc_resolver`, `oxc_semantic`, `oxc_minifier`, and parallel Rust compression.
- 💾 **Persistent Caching:** Results are cached in-memory and to disk using `papaya` and `redb`, with startup prewarm for recent entries.
- 🧩 **Multi-Framework Support:** Native support for JavaScript, TypeScript, JSX/TSX, `.mts`, `.cts`, Svelte (`<script>` blocks), and Astro (frontmatter and client scripts).
- 🎨 **Flexible UI Options:** Displays colored inline hints by default, with native accessible Inlay Hints, end-of-line decorations, and CodeLens annotations available.
- 📈 **Impact Insights:** Shows confidence levels, working-tree import cost deltas, per-import history trends, current-file totals, shared dependency explanations, and barrel re-export warnings.
- 🛠️ **Import Actions:** Offers tree-shaking CodeActions, local substitution suggestions, diagnostic copy actions, named export candidates and completions, bundle history, and workspace reports.
- 🧭 **Guidance Workflows:** Adds package.json dependency CodeLens, import comparison, `.importlensignore`, and opt-in npm registry hints that fail silently when unavailable.
- 🧾 **Operational Visibility:** The ImportLens output channel records daemon startup, IPC, fallback, cache, and troubleshooting events according to `importLens.logLevel`.
- 🪶 **Runtime-Aware Results:** Declaration-only packages report zero runtime bytes, framework virtual modules are skipped, and conservative CJS or fallback paths surface structured diagnostics instead of silently failing.

## How It Works

ImportLens analyzes your import statements and resolves the exact package version installed in your local `node_modules`. It then constructs a virtual module graph, tree-shakes dead code using custom reachability analysis, minifies the output, and compresses it in parallel (Gzip, Brotli, Zstd).

All of this happens invisibly in a secure, self-contained background daemon, meaning your editor stays responsive. Results are keyed by the active document path, so nested workspaces, pnpm layouts, and loose files opened outside a VS Code workspace can still resolve from the nearest usable package tree.

## Supported Languages
- JavaScript (`.js`, `.mjs`, `.cjs`)
- TypeScript (`.ts`, `.mts`, `.cts`)
- React & Solid (`.jsx`, `.tsx`)
- Svelte (`.svelte`)
- Astro (`.astro`)

> **Note:** Framework virtual modules and common app aliases (`astro:*`, `virtual:*`, `$app/*`, `$env/*`, `@/*`) are automatically ignored as they are not npm package dependencies.

## Editor Insights

ImportLens adds context next to size labels when the extra signal is useful:

- **Confidence colors:** High-confidence sizes use a muted success color, medium confidence uses amber, and low confidence uses red. Low-confidence inline labels also start with `~`, for example `~1.6 kB br`.
- **Budgets:** Optional per-import and per-file Brotli thresholds surface as editor diagnostics, inline `over budget` labels, hovers, reports, and CLI failures.
- **Working-tree deltas:** Imports added or modified in the current Git diff show their current added Brotli cost, for example `+2.1 kB br`.
- **History trends:** Repeated measurements can show when an import became larger or smaller after dependency updates.
- **Shared bytes:** When multiple imports in the same file include the same module path, hovers and reports explain the shared cost.
- **Barrel re-exports:** `export * from "package"` is flagged because it keeps the package boundary broad and can prevent precise named-export tree-shaking.
- **Tree-shaking actions:** For CommonJS, side-effectful, namespace, or otherwise non-tree-shakeable imports, lightbulb actions can inspect or copy diagnostics. Namespace imports can also enumerate named exports and copy a named import candidate.
- **Substitution suggestions:** Curated local alternatives for known heavy packages appear as copy-only CodeActions. ImportLens never rewrites source automatically.
- **Package dependency lenses:** `package.json` dependency entries can show their ImportLens Brotli size and open the compare workflow.

ImportLens does not rewrite source files automatically. Actions that suggest named imports or package substitutions copy a candidate to the clipboard so you stay in control of usage changes.

## Commands

| Command | Description |
| --- | --- |
| `ImportLens: Show Current File Size` | Calculates a deduplicated total for runtime package imports in the active file and records it in bundle impact history. |
| `ImportLens: Show Bundle Impact History` | Opens a script-free SVG history panel from recent current-file measurements in VS Code global storage. |
| `ImportLens: Show Report` | Scans the workspace and opens a report of imports sorted by Brotli size, with duplicate imports, shared modules, budget counts, and an SVG treemap. |
| `ImportLens: Compare Imports` | Compares comma-separated package imports from the active workspace and lists them by Brotli size. |
| `ImportLens: Clear Cache` | Clears daemon memory and disk cache, then reanalyzes the active document. |
| `ImportLens: Show Logs` | Opens the ImportLens output channel. |

## Configuration

ImportLens is highly customizable to fit your workflow. You can tweak these settings in your VS Code `settings.json`:

| Setting | Description |
| --- | --- |
| `importLens.display` | Set the display mode: `inlayHint` (default), `minimal`, `standard`, or `verbose`. |
| `importLens.inlineRenderer` | Choose the renderer for `display: inlayHint`: `colored` (default) for confidence-colored decoration-backed hints, or `native` for VS Code's screen-reader-accessible Inlay Hints API. |
| `importLens.compression` | The primary compression size to display: `brotli` (default), `gzip`, `zstd`, or `all`. |
| `importLens.budgets` | Optional budget object with `perImportBrotliBytes` and `perFileBrotliBytes` thresholds. |
| `importLens.enableRegistryHints` | Opt in to short-timeout npm metadata hints in package.json CodeLens (`false` by default). |
| `importLens.enableDiskCache` | Enable persistent caching to disk (`true` by default). |
| `importLens.useCodeLens` | Show sizes as a CodeLens above the import instead of inline (`false` by default). |
| `importLens.showWarnings` | Show warning indicators when a package cannot be efficiently tree-shaken. |
| `importLens.logLevel` | Controls output-channel verbosity: `error`, `warn`, `info`, or `debug` (`info` by default). |

## Diagnostics & Troubleshooting

If ImportLens cannot determine the size of a package, it will show an `unavailable` hint. 
Hover over the import statement and click **Copy diagnostics** to extract the detailed, structured error context directly from the Rust daemon for easy debugging.

CommonJS-only packages, packages with conservative `sideEffects` metadata, and imports that are not truly tree-shakeable include confidence and diagnostic details in hovers, reports, copied diagnostics, and debug logs. The normal output channel warning level is reserved for daemon, IPC, startup, protocol, or no-result failures.

## CLI Budget Check

Run `importlens check` from a workspace to check changed JS/TS files from `git diff HEAD` against configured budgets. Budgets can live in `.importlensrc.json` as `{ "budgets": { ... } }` or in `package.json` as `{ "importLens": { "budgets": { ... } } }`. The CLI uses the native daemon for real Brotli sizes and exits non-zero when a threshold is exceeded.

## Ignoring Imports

Create a `.importlensignore` file to skip known generated files or imports:

```text
package:large-package
import:@internal/*
path:src/generated/**
```

## Requirements

- VS Code version 1.100.0 or higher.
- A local workspace or loose file whose parent tree contains a populated `node_modules` directory.

---
*Built with [OXC](https://oxc.rs/) for maximum performance.*
