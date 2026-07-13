# The result model: a size exists if and only if a build succeeded

Every import analysis is in exactly one of three states, and the state is legible from the
result's *type*, not from a convention a reader has to know:

| State | Sizes | Meaning |
| --- | --- | --- |
| **Measured** | present | A build succeeded. |
| **Loading** | none *yet* | A build is in flight. The response did not wait for it. |
| **Unmeasured** | none *ever* | The build could not answer. Carries a **stage**. |

**Unmeasured splits by cause**, and this distinction is the whole point:

- **Deterministic** — `parse`, `link`, `missing_export`, `ambiguous_export`, `output_shape`,
  `module_graph_limit`. A property of the package's **bytes**. Same input, same outcome.
- **Transient** — `panic`, `timeout`, `engine_gone`. A property of **this moment's
  scheduling**. It says nothing at all about the package.

## Why this ADR exists

The daemon shipped a **fabricated** state that this table has no room for: a build would fail,
a static fallback would invent a plausible size, and the result would carry **`error: None`
plus that size**. Every consumer in the system asks "is this result usable?" the same way —

```
!result.error          // budgets.ts, insights.ts, cli/importlens.mjs
result.error.is_none() // report/model.rs, should_cache_result
```

— a **negative check on `error`**. The fabricated state passes all of them.

That is not a bug. It is **one missing model, replicated everywhere anyone needed to ask the
question**, and it produced the same defect six times in six different places, each found only
after the previous fix shipped: a circuit breaker condemning a healthy package for a whole
cache generation; a degraded 58-byte fallback cached over a healthy 17,550-byte package; an
incomplete file total cached for its TTL; a fabricated size written to the persisted
import-cost history, destroying that import's real baseline; a fabricated *import count* in the
bundle-impact history; and — worst — `importlens check` deciding **CI pass/fail** from a
fabricated size, silently passing, so the regression merges.

## The invariants

1. **A size exists if and only if a build succeeded.** There is no fabricated size anywhere in
   the system. This is [ADR-0003](0003-no-size-without-a-build.md), applied without exception.
2. **The question a consumer asks is "is there a size?"** — an `Option`, enforced by the
   compiler and by the type system on the wire. **Never "is there an error?"** Invariant 1 is
   what makes this safe: a degraded result has no size to misuse.
3. **A transient outcome may never enter a durable store.** Durable means: the L1 and L2
   caches, every memo, the extension's `workspaceState` and `globalState` histories, and **any
   pass/fail verdict**. A deterministic outcome **may** be cached — it is a property of the
   bytes, and the cache is already keyed by those bytes' fingerprints, so it expires exactly
   when the answer would change. Not caching it would re-enter the engine for a broken package
   on every analysis, forever, burning one of only two permits.
4. **An aggregate is only as complete as its inputs.** If any contributing import is Loading or
   Unmeasured, the total is a **floor**: flagged `incomplete`, never cached, never persisted,
   never compared against a baseline.
5. **No verdict from a floor.** A budget is never judged against an incomplete number — not
   "pass", not "fail". *"Not evaluated."* And **a gate that cannot measure must never report
   success**: `importlens check` exits non-zero with a distinct code, so a flaky CI box is
   diagnosable and is never confused with a genuine regression. A silent pass is the worst
   outcome available, because it merges the regression.

## Consequences

- Coverage drops: an import whose manifest cannot be parsed, whose entry exceeds the module
  source limit, or whose build fails, shows "could not measure" instead of a number. Accepted.
- **Loading and Unmeasured are different states and every consumer must distinguish them.**
  "No size yet" is not "no size ever".
- The invariants must be **guarded**, not merely documented — six rounds of documentation did
  not prevent the seventh instance:
  - a **Guard** test banning the negative-`error` usability check in size-consuming code;
  - a **Property** test over *every* durable store: feeding it a transient result is rejected;
  - the daemon must not be *able* to construct a result carrying both a size and a transient
    stage.
- An honest **lower bound** on a failed build ("at least 4 MB; graph limit exceeded") remains
  the intended successor to Unmeasured — a limit breach means much of the graph *was* loaded
  before we stopped. It needs the partial graph plumbed through the engine boundary, and it
  does not belong inside this change.
