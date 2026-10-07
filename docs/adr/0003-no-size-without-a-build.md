# If Rolldown did not build it, we report no size

An import whose graph Rolldown could not build reports **no byte count at all** — it reports
that it could not be measured, and why. Import Lens previously substituted an approximation
in three places: an unreadable package manifest fell back to the package's size *on disk*,
an oversized entry file fell back to sizing *that file alone*, and any engine build failure
fell back to the same. Each produced a plausible-looking number that was not an Import Cost:
a directory size overstates by including tests, source maps and unused files; a lone entry
file understates by ignoring the entire graph it pulls in. A large UI kit that breached a
graph limit was reported at the few kilobytes of its barrel file, when the true answer was
megabytes.

A confidence badge does not fix this. Users read the byte count. A number that is wrong by
an order of magnitude while looking like a measurement is worse than no number, because it
is *actionable* and the action is wrong.

## Consequences

- Coverage drops: imports whose manifest cannot be parsed, whose entry exceeds the module
  source limit, or whose build fails now show "could not measure" instead of a size. This is
  accepted.
- Types-only and declaration-only packages are measured at zero (`pipeline/types_only.rs`).
  The `sideEffects` glob matcher stays on the successful-measurement path as a reporting-only
  badge source ([ADR-0002](0002-upstream-owns-everything-it-can-answer.md)); it never affects
  retention or size.
- A graph-limit breach is not a size either. A partial graph gives no honest lower bound: which modules loaded before the limit tripped depends on scheduling, so the figure would change from run to run, and cutting the graph removes the cross-module facts that let the full build drop code, so it is not even a floor. A breach instead retries with Rolldown's lazy barrel, which measures a named import from a side-effect-free barrel exactly; an import that genuinely reaches more than the limit stays unmeasured (known-issues D2 holds the measurements).
