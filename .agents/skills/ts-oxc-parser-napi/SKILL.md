---
name: ts-oxc-parser-napi
description: "Synchronous ESM import extraction using the oxc-parser NAPI binding (v0.133.0) in TypeScript. Use when implementing extension/src/parser.ts (FR-004, FR-005)."
---

# Instructions

When parsing JavaScript or TypeScript files to extract imports in the extension host, you must use the native `oxc-parser` NAPI binding.

> [!WARNING]
>
> - Do **NOT** use `@oxc-parser/wasm`. It is officially deprecated and banned (§9.4.4).
> - Do **NOT** use the TypeScript Compiler API.
> - The npm `oxc-parser` version MUST be `0.133.0` - matching the Rust-side `oxc_parser` crate version. Both are released from the same OXC monorepo.

## 1. Syntax

Use `parseSync` to extract imports synchronously. It returns an AST and `module.staticImports` directly, avoiding manual AST traversal.

```typescript
import { parseSync } from "oxc-parser";

export interface ExtractedImport {
  specifier: string;
  importKind: "named" | "default" | "namespace" | "dynamic";
  named: string[];
}

export function extractImports(
  sourceText: string,
  filePath: string,
): ExtractedImport[] {
  const result = parseSync(filePath, sourceText, {
    sourceType: "module",
  });

  // oxc-parser provides standard ESM imports natively via result.module
  if (!result.module || !result.module.staticImports) {
    return [];
  }

  const imports: ExtractedImport[] = [];

  for (const imp of result.module.staticImports) {
    // Skip type-only imports — zero runtime cost (FR-001)
    if (imp.isType) continue;

    // Skip relative imports (FR-002)
    if (
      imp.moduleRequest.startsWith("./") ||
      imp.moduleRequest.startsWith("../")
    )
      continue;

    // Skip node builtins (FR-003)
    if (
      imp.moduleRequest.startsWith("node:") ||
      isNodeBuiltin(imp.moduleRequest)
    )
      continue;

    // Map to our ExtractedImport interface based on import shape
    imports.push(mapToExtractedImport(imp));
  }

  return imports;
}
```

## 2. Error Recovery (FR-005)

OXC's error recovery mode is enabled by default. When the user is mid-typing an incomplete import statement, the parser extracts as much structural information as possible rather than failing. You should:

- Always attempt extraction even if `result.errors` is non-empty.
- Render partial results if a package name can be identified.
- Suppress decorations silently if no package name can be resolved.

## 3. Import Kinds Mapping

| Source pattern                   | `importKind` | `named` array    |
| -------------------------------- | ------------ | ---------------- |
| `import { a, b } from 'pkg'`     | `named`      | `['a', 'b']`     |
| `import Foo from 'pkg'`          | `default`    | `[]`             |
| `import * as Pkg from 'pkg'`     | `namespace`  | `[]`             |
| `import('pkg')`                  | `dynamic`    | `[]`             |
| `export { a } from 'pkg'`        | `named`      | `['a']`          |
| `import type { Foo } from 'pkg'` | —            | Skipped entirely |

## Rules

- This skill applies to VS Code Desktop only (Tier 1 & 2). NAPI addons do NOT work in browser environments — VS Code for the Web enters degraded mode with no parsing.
- Compile with TypeScript 6.x using `module: "esnext"` and `moduleResolution: "bundler"`.
- This is a synchronous call — it runs on the extension host thread. It is fast (<5ms for typical files) but must not be called in a tight loop.
