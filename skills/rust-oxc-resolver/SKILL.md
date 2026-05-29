---
name: rust-oxc-resolver
description: "Resolving module entry paths from the active document path using oxc_resolver (v11.19.x, separate repo from OXC monorepo). Use when implementing daemon/src/pipeline/resolve.rs (FR-008, FR-017)."
---

# Instructions

To locate the actual source code of `package.json` entry points for tree-shaking, we rely on `oxc_resolver` (v11.x).

## 1. Context Directory Target

Because of issues with PNPM monorepos and nested dependency versions, you must **not** resolve packages starting from the workspace root.

Instead, resolve packages starting from the **active document's path** provided by the extension host.

```rust
use std::path::Path;
use oxc_resolver::{ResolveOptions, Resolver};

// Active document path from the IPC BatchRequest
let active_doc_dir = Path::new(&request.active_document_path).parent().unwrap();

let options = ResolveOptions::default();
let resolver = Resolver::new(options);

// e.g., resolving "lodash-es" from src/components/Button.tsx
let resolution = resolver.resolve(active_doc_dir, &request.imports[0].specifier);

if let Ok(res) = resolution {
    let resolved_path = res.full_path();
}
```

## Rules

- Note that `oxc_resolver` lives in a separate repository (`oxc-project/oxc-resolver`) and is versioned independently from the main OXC suite. Use `~11.19`.
- Cache resolver instances rather than recreating them on every request, as instances load and parse package.json files natively.
