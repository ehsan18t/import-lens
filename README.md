# ImportLens

ImportLens is a blazingly fast Visual Studio Code extension that displays the real-world post-bundle cost of your npm imports directly inline as you type.

Unlike existing import cost calculators that spin up heavy Node.js bundlers, ImportLens offloads all computation to a highly optimized background Rust daemon powered by the **OXC** (Oxidation Compiler) toolchain. It performs real tree-shaking, minification, and compression in milliseconds without blocking your editor or consuming massive amounts of memory.

## Features

- ⚡ **Instant Feedback:** See post-tree-shake, minified, and compressed (Gzip, Brotli, Zstd) sizes inline.
- 🌳 **Real Tree-shaking:** Accurately calculates sizes for named, default, namespace, and dynamic imports.
- 🦀 **Rust Daemon Engine:** Built on `oxc_parser`, `oxc_semantic`, and `oxc_minifier` for unparalleled speed.
- 💾 **Persistent Caching:** Results are cached in-memory and to disk using a fast embedded database (`redb`), making repeat lookups instantaneous.
- 🧩 **Multi-Framework Support:** Native support for JavaScript, TypeScript, JSX/TSX, Svelte (`<script>` blocks), and Astro (frontmatter and client scripts).
- 🎨 **Flexible UI Options:** Displays sizes as accessible **Inlay Hints** (default), end-of-line decorations, or CodeLens annotations.

## How It Works

ImportLens analyzes your import statements and resolves the exact package version installed in your local `node_modules`. It then constructs a virtual module graph, tree-shakes dead code using custom reachability analysis, minifies the output, and compresses it in parallel (Gzip, Brotli, Zstd).

All of this happens invisibly in a secure, self-contained background daemon, meaning your editor stays buttery smooth.

## Supported Languages
- JavaScript (`.js`, `.mjs`, `.cjs`)
- TypeScript (`.ts`, `.mts`, `.cts`)
- React & Solid (`.jsx`, `.tsx`)
- Svelte (`.svelte`)
- Astro (`.astro`)

> **Note:** Framework virtual modules and common app aliases (`astro:*`, `virtual:*`, `$app/*`, `$env/*`, `@/*`) are automatically ignored as they are not npm package dependencies.

## Configuration

ImportLens is highly customizable to fit your workflow. You can tweak these settings in your VS Code `settings.json`:

| Setting | Description |
| --- | --- |
| `importLens.display` | Set the display mode: `inlayHint` (default), `minimal`, `standard`, or `verbose`. |
| `importLens.compression` | The primary compression size to display: `brotli` (default), `gzip`, `zstd`, or `all`. |
| `importLens.enableDiskCache` | Enable persistent caching to disk (`true` by default). |
| `importLens.useCodeLens` | Show sizes as a CodeLens above the import instead of inline (`false` by default). |
| `importLens.showWarnings` | Show an indicator `(!)` when a package cannot be efficiently tree-shaken. |

## Diagnostics & Troubleshooting

If ImportLens cannot determine the size of a package, it will show an `unavailable` hint. 
Hover over the import statement and click **Copy diagnostics** to extract the detailed, structured error context directly from the Rust daemon for easy debugging.

## Requirements

- VS Code version 1.100.0 or higher.
- A local workspace or loose file whose parent tree contains a populated `node_modules` directory.

---
*Built with [OXC](https://oxc.rs/) for maximum performance.*
