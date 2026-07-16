# If Rolldown did not build it, we report no size

An import whose graph Rolldown could not build reports **no byte count at all** — it reports
that it could not be measured, and why. Import Lens previously substituted an approximation
in three places: an unreadable package manifest fell back to the package's size *on disk*
(`analyze.rs:157`), an oversized entry file fell back to sizing *that file alone*
(`analyze.rs:227`), and any engine build failure fell back to the same
(`analyze.rs:260`). Each produced a plausible-looking number that was not an Import Cost:
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
- The static-analysis path retains a purpose only for genuinely size-free results (types-only
  and declaration-only packages). The side-effect glob matcher quarantined there by the I9
  amendment loses most of its remaining reach.
- An honest **lower bound** ("at least 4 MB; graph limit exceeded") is strictly better than
  either a fabrication or a blank, because a limit breach means much of the graph *was* loaded
  before we stopped. The engine currently discards the partial graph on failure, so this needs
  plumbing through the engine boundary and is deliberately not bundled into a stability fix.
  It is the intended successor to this decision, not a hypothetical.
