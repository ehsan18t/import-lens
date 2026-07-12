# Import Lens

Import Lens tells a developer what a single `import` statement costs. It is an
import-cost tool, not a bundle-size tool — everything in this glossary follows from
that distinction.

## Language

### The unit of measurement

**Import Cost**:
The bytes a single import contributes to an application that is otherwise empty. Every
number the product reports is an Import Cost or an arithmetic view over Import Costs. It
prices the package as published, under no project's build configuration — see
[ADR-0001](docs/adr/0001-measure-a-neutral-build.md). It currently counts JavaScript only;
non-JavaScript bytes a package ships (CSS, wasm, fonts) are real cost, are not yet included,
and must be **disclosed on the result** rather than silently omitted.
_Avoid_: bundle size, footprint

**Bundle Size**:
What an application actually ships, after every import across every file is unioned and
deduplicated. Import Lens does **not** measure this and has never intended to. No figure
the product displays may be named or framed as one.
_Avoid_: total size, project size

**Combined Import Cost**:
The sum of the Import Costs of a set of imports, each counted independently. A dependency
shared across files is counted at every site, so this is an upper bound on what those
imports would ship together — it ranks and it apportions blame, but it is never a Bundle
Size. The workspace report's headline figure is this.
_Avoid_: total, total brotli, total size

**File Cost**:
What one source file's imports cost together, built as a single bundle so a module reached
by two of its imports is counted once. Still priced against an otherwise-empty application,
so it is an Import Cost at file granularity — not a Bundle Size. The status bar shows this,
and the per-file budget gates on it.
_Avoid_: file total, page weight

**Runtime**:
The condition set an import resolves under — Server, Client, or Component. A runtime is an
**artifact boundary**: two runtimes in one file are two things that ship, each carrying its
own copy of anything both need. Costs are therefore measured per runtime and added across
runtimes; nothing is ever deduplicated across a runtime boundary.

**Shared Module**:
A module reached by more than one import *within a single runtime*, and so linked and paid
for once. A module reached from two different runtimes is not shared — each runtime ships
its own copy — and must never be reported as a saving.
_Avoid_: common dependency, deduplicated module

**Side-Effectful**:
A property of *the import*, not of the package it comes from: the entry file being measured
is one the package declares as having side effects. A package declaring
`"sideEffects": ["**/*.css"]` is not side-effectful for a JavaScript import — the rule says
nothing about that entry.

**Truly Tree-Shakeable**:
An import whose measured cost is meaningfully below the cost of importing its whole package
(currently: at most 95% of it). It is a claim about what the user *saved* by importing
narrowly, and it is only asked of a named, non-Side-Effectful import.
_Avoid_: tree-shakes, shakeable

**Unmeasured**:
The state of an import whose graph Rolldown could not build — a failed build, a manifest we
cannot parse, an entry over the module source limit. An Unmeasured import has **no size**.
It is not a size of zero, and it is not an estimate carrying a low-confidence badge.
_Avoid_: fallback size, approximate size, conservative estimate

**Marginal Cost**:
What adding an import to a project that already contains some of its dependencies would
cost. Requires a Bundle Size model, so it is outside this context. Named here only so
that "what does this really cost me" questions resolve to a term the product does not
implement.
