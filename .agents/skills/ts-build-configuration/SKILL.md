---
name: ts-build-configuration
description: "TypeScript 6.x tsconfig.json setup and tsdown bundler configuration for the VS Code extension host. Use when creating or modifying build configuration files."
---

# Instructions

To guarantee compatibility with the upcoming TypeScript 7.0 (Corsa) and to keep the bundle size minimal, we use TypeScript 6.x and the `tsdown` bundler.

## 1. TypeScript Configuration (tsconfig.json)

You **must** use the modern TypeScript 6.x defaults. Do not use legacy module systems or resolution strategies.

```json
{
  "compilerOptions": {
    "target": "es2025",
    "module": "esnext",
    "moduleResolution": "bundler",
    // CRITICAL: TS 6 no longer auto-includes @types. You must be explicit or use []
    "types": [],
    "strict": true,
    "isolatedDeclarations": true,
    "noUncheckedSideEffectImports": true,
    "skipLibCheck": true,
    "outDir": "./dist"
  },
  "include": ["src/**/*"]
}
```

## 2. Bundler Configuration (tsdown.config.ts)

We use `tsdown` (which is powered by Rolldown/Oxc) to compile the extension host into a single file. Wait, VS Code extensions require CommonJS (`cjs`) to be executed by the Node.js extension host environment.

```typescript
import { defineConfig } from "tsdown";

export default defineConfig({
  entry: ["./src/extension.ts"],
  format: ["cjs"],
  clean: true,
  minify: true,
  // DO NOT bundle the vscode API!
  external: ["vscode"],
  // No need for dts for an extension binary
  dts: false,
});
```

## Rules

- NEVER use `target: "es5"` or `moduleResolution: "node"` (Node10 behavior). They are deprecated in TS 6.
- The `vscode` module is provided at runtime by the editor. It must always be marked as `external` in the bundler config, otherwise the build will fail or the extension will crash.
