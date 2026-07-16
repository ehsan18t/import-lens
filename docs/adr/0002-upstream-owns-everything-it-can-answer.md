# Upstream owns everything it can answer; hand-written logic is the last resort

Rolldown and OXC are the authorities for anything they are capable of deciding — resolution,
module retention, tree-shaking, side-effect semantics, parsing, minification, glob matching.
We do not re-derive an answer an upstream crate can give us. The reasons are that a
reimplementation is a second, worse implementation that silently disagrees with the first,
and that these projects are maintained by far more people, far more expert, than we can put
on the problem. Hand-written logic is what we reach for when upstream genuinely cannot answer
— never merely because writing it is quicker.

Two rules follow:

1. **Where we pre-resolve something for Rolldown, we supply it the metadata it would have
   found itself.** The entry module is resolved by our plugin, so it must be handed a
   `package_json_path` alongside the resolved id — otherwise Rolldown falls back to a
   degraded answer through no fault of its own.
2. **Where upstream already vendors a component, we use *that* component**, not a lookalike.
   Side-effect glob matching went through ~80 hand-written lines of brace expansion and
   segment matching, while `fast-glob` — the very matcher Rolldown uses
   (`rolldown_utils::pattern_filter`) — was already in our `Cargo.lock`. Two glob engines
   reading one `sideEffects` array can disagree, and then we label a file that Rolldown
   treated the opposite way. Using Rolldown's own matcher makes that class of bug impossible
   rather than unlikely.

## The exception: metadata upstream will not expose

In Rolldown 1.1.5, `ModuleInfo` — everything a plugin can learn about a module — carries
`code`, `id`, `is_entry`, `importers`, `imported_ids`, `exports` and `input_format`, but
**nothing about side effects**; the real classification (`DeterminedSideEffects`) lives on
internal module types and reaches no output type. So the Side-Effectful badge cannot be
sourced from Rolldown and must come from our own manifest reader.

That reader is therefore **reporting-only**: it may label a result, but it must never
influence what Rolldown retains. A size always reflects Rolldown's decisions; a badge may
reflect ours. Anyone proposing to delete the reader should first re-check `ModuleInfo` in the
current Rolldown — if a release ever exposes the classification, the reader goes.

## Consequences

- `fast-glob` is **exact-pinned as part of the coordinated compiler stack**, not floated as an
  ordinary dependency. Its whole purpose is to agree with Rolldown; if Cargo resolves us to a
  different version than `rolldown_utils` got, that agreement breaks silently — which is the
  exact failure the pinning policy exists to prevent. It joins the compiler-stack fingerprint.
- Swapping the hand-written matcher for `fast_glob` is a **behaviour change, not a refactor**:
  the two engines disagree on some patterns, which is the reason for the swap. The
  real-package badge baseline must exist before it lands, or nobody will see what moved.
