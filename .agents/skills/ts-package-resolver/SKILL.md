---
name: ts-package-resolver
description: "Resolving installed package versions from node_modules, handling scoped packages and monorepos. Use when implementing extension/src/resolver.ts (FR-007, FR-008)."
---

# Instructions

Before sending a `BatchRequest` to the daemon, the extension host must resolve the installed version of each imported package from `node_modules`.

## 1. Version Resolution

For each import specifier, read the `version` field from the package's `package.json` in `node_modules`:

```typescript
import * as fs from "fs";
import * as path from "path";

interface ResolvedPackage {
  name: string;
  version: string;
  packageJsonPath: string;
}

export async function resolvePackageVersion(
  specifier: string,
  documentDir: string,
): Promise<ResolvedPackage | null> {
  // Extract the package name from the specifier
  const packageName = extractPackageName(specifier);
  if (!packageName) return null;

  // Walk up from the document's directory to find node_modules
  let dir = documentDir;
  while (dir !== path.dirname(dir)) {
    const pkgJsonPath = path.join(
      dir,
      "node_modules",
      packageName,
      "package.json",
    );
    try {
      const content = await fs.promises.readFile(pkgJsonPath, "utf-8");
      const pkg = JSON.parse(content);
      return {
        name: packageName,
        version: pkg.version,
        packageJsonPath: pkgJsonPath,
      };
    } catch {
      // Not found at this level, walk up
      dir = path.dirname(dir);
    }
  }
  return null; // Package not found
}
```

## 2. Scoped Packages

Scoped packages (e.g., `@babel/core`, `@tanstack/react-query`) have a directory structure like:

```
node_modules/@babel/core/package.json
```

Extract the package name correctly:

```typescript
function extractPackageName(specifier: string): string | null {
  if (specifier.startsWith("@")) {
    // Scoped package: @scope/name or @scope/name/subpath
    const parts = specifier.split("/");
    if (parts.length < 2) return null;
    return `${parts[0]}/${parts[1]}`;
  }
  // Unscoped: name or name/subpath
  return specifier.split("/")[0];
}
```

## 3. Specifier Filtering (FR-002, FR-003)

Skip these specifiers — they don't come from `node_modules`:

- **Relative paths**: `./utils`, `../helpers`
- **Node builtins**: `node:fs`, `node:path`, `fs`, `path`, `crypto`, etc.
- **Type-only imports**: `import type { Foo } from 'bar'` (already filtered by parser)

```typescript
const NODE_BUILTINS = new Set([
  "assert",
  "buffer",
  "child_process",
  "cluster",
  "console",
  "constants",
  "crypto",
  "dgram",
  "dns",
  "domain",
  "events",
  "fs",
  "http",
  "http2",
  "https",
  "module",
  "net",
  "os",
  "path",
  "perf_hooks",
  "process",
  "punycode",
  "querystring",
  "readline",
  "repl",
  "stream",
  "string_decoder",
  "sys",
  "timers",
  "tls",
  "trace_events",
  "tty",
  "url",
  "util",
  "v8",
  "vm",
  "worker_threads",
  "zlib",
]);

function shouldSkip(specifier: string): boolean {
  if (specifier.startsWith(".")) return true;
  if (specifier.startsWith("node:")) return true;
  const bare = specifier.split("/")[0];
  return NODE_BUILTINS.has(bare);
}
```

## 4. Monorepo Awareness

In monorepos, packages may be nested at different levels. Always resolve starting from the **document's directory** and walk upward, not from the workspace root. This correctly handles:

- Hoisted dependencies (at workspace root `node_modules`)
- Package-level dependencies (at package `node_modules`)
- PNPM's `.pnpm` directory structure

## Rules

- Never cache the `version` string permanently — it can change when the user runs `npm install`.
- If a package version cannot be resolved, the import should be displayed with "Package not found" rather than throwing an error.
- Read `package.json` asynchronously (`fs.promises.readFile`) to avoid blocking the extension host.
