---
name: rust-module-graph-walker
description: "Custom tree-shaking via module graph reachability analysis using oxc_parser + oxc_resolver + oxc_semantic. Use when implementing daemon/src/pipeline/graph.rs and treeshake.rs (FR-018)."
---

# Instructions

OXC does NOT supply a standalone tree-shaker. ImportLens implements its own module graph walker to estimate sizes for named imports.

## 1. Virtual Entry Construction

Given a named import request like `import { debounce } from 'lodash-es'`, build an in-memory string:

```javascript
// Named imports
export { debounce } from "lodash-es";

// Default import
export { default } from "react";

// Namespace import
export * from "lodash-es";

// Dynamic import: resolve the package entry point directly
// and pass it to the pipeline without a virtual entry file
```

> [!WARNING]
> Do NOT use `console.log` or any pattern that can be statically eliminated by a tree-shaker in virtual entries. Re-exports are semantically unambiguous — the bundler cannot drop a named export from an entry module.

## 2. Tree/Reachability Walking

Use `oxc_semantic` (v0.138.0) to trace AST scope bounds, build symbol tables, and trace reference identifiers across ES Module files.

```rust
use oxc_semantic::SemanticBuilder;

let semantic_ret = SemanticBuilder::new()
    .build(&program);

let semantic = semantic_ret.semantic;
let symbol_table = semantic.symbols();
```

Use `symbol_table` coupled with `oxc_resolver` to walk module boundaries. Mark any nodes not referenced by the virtual entry exports as dead code.

The pipeline for each module:

1. Parse with `oxc_parser` (arena-allocated AST).
2. Run `oxc_semantic` for scope trees, symbol tables, binding info.
3. Walk the module graph from the virtual entry's requested exports.
4. Mark all transitively reachable code.
5. Concatenate only the reachable code into a single in-memory source.

## 3. The `truly_treeshakeable` Heuristic

After computing the size for the named exports, you **must run a second pass** against the entire package entry point.

If `(named_export_size / full_package_size) >= 0.95`, then the internal dependencies of that package are heavily tangled. When this condition is met, respond with `truly_treeshakeable: false`.

## 4. sideEffects Handling (FR-021) — IMPORTANT

Before tree-shaking, read the `sideEffects` field from the package's `package.json`:

- **`true` or absent**: Set `side_effects: true`. Tree-shake conservatively.
- **`false`**: Set `side_effects: false`. Aggressive tree-shaking permitted.
- **Array of glob patterns** (e.g., `["*.css", "dist/polyfill.js"]`): Treat conservatively as `side_effects: true` for v1.0. Full glob-array evaluation is deferred to v1.1.

## 5. Dynamic Import Limitation

`const { format } = await import('date-fns')` — named bindings on dynamic imports require runtime analysis. In v1.0, these are treated as full module entry size (same as bare `import('date-fns')`). Document this in comments.

## Rules

- Do NOT use the unstable `rolldown_core` Rust crate APIs (C-003).
- All OXC monorepo crates (`oxc_parser`, `oxc_semantic`, `oxc_codegen`, etc.) must be patch-pinned (`~`) to ONE coordinated version — currently **0.138.0** — so they always share a SINGLE version. `oxc_resolver` is versioned independently at **v11.22.x**.
- Use explicit reachability bounds. The module graph walker assumes re-exports from virtual files are strictly preserved.
