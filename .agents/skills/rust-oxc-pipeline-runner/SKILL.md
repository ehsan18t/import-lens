---
name: rust-oxc-pipeline-runner
description: "Orchestrating the native OXC AST pipeline: parse, transform, minify, and codegen. All crates pinned to v0.138.0. Use when implementing daemon/src/pipeline/ (FR-016-FR-019, FR-024)."
---

# Instructions

The OXC Pipeline (v0.138.0) requires a shared AST `Allocator` bound to a specific lifetime sequence.

## 1. Setup the Allocator

```rust
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;

let allocator = Allocator::default();
```

## 2. Parse, Transform, and Minify

> [!WARNING]
> `oxc_minifier` is in Alpha. If integration tests fail, we skip it (`(no-minify)` label) but the rest of the AST pipeline must still function.

```rust
use oxc_codegen::Codegen;
use oxc_minifier::{Minifier, MinifierOptions};
use oxc_transformer::Transformer;

// 1. Parsing
let ret = Parser::new(&allocator, source_text, SourceType::mjs()).parse();
let mut program = ret.program;

// 2. Transformer (Strip TS/JSX - does NOT tree-shake!)
// Transformer::new...

// 3. Minifier (Alpha) -> Note: Minifier::new takes Options and applies against allocator
// Mangling metadata is produced here by the minifier; there is no separate mangler crate.
let minifier_options = MinifierOptions::default();
Minifier::new(minifier_options).minify(&allocator, &mut program);

// 4. CodeGen
// Codegen::new() converts the minified AST back to a string.
// minify: true removes whitespace.
let minified_string = Codegen::new().with_minify(true).build(&program).code;
```

## Rules

- The allocator **must not** be dropped before `Codegen::build()` completes.
- Never use `oxc_transformer` to attempt tree shaking; it does not natively support DCE (Dead Code Elimination).
- Always ensure `oxc_allocator`, `oxc_span`, `oxc_parser`, `oxc_semantic`, `oxc_codegen`, and `oxc_minifier` share the exact same version string (0.138.0), as AST node versions are unstable between minor releases in OXC.
