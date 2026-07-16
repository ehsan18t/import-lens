# Import Lens measures imports, not bundles

The unit of measurement is the **import**, priced as though the application were otherwise
empty. Import Lens deliberately has no model of "what is already in the bundle", and never
had one — deduplicating a project's imports into a union was not a deferred feature, it was
never the product.

Everything follows from this:

- The workspace report's headline is a **Combined Import Cost** — the sum of independent
  Import Costs, which counts a dependency shared across fifty files fifty times. It was
  previously labelled "Total Brotli", which a reader inevitably reads as "what my project
  ships". The arithmetic was never wrong; the word "Total" was. It is a ranking and
  blame-apportioning figure, not a size.
- The report computes `duplicate_imports` and `shared_modules` and **does not subtract them
  from the total**. That is correct, not an oversight: subtracting them would be asserting a
  bundle-level quantity we do not model.
- A **File Cost** — one document's imports built as a single bundle, so a module reached by
  two of them is counted once — *is* a legitimate quantity, because it is still priced against
  an empty application. The status bar shows it and the per-file budget gates on it. It had
  been re-derived by *summing* per-import bytes, which produced false "budget exceeded"
  warnings on files that were inside budget, while the correct number sat on screen one line
  away.

## Consequences

- The file budget (File Cost) and the report's ranking (Combined Import Cost) will disagree
  for the same file. They answer different questions. This is intended.
- Compressed figures are **not additive**: brotli(A) + brotli(B) exceeds brotli(A ∪ B), because
  compression finds redundancy across the union it cannot see in the parts. Summing compressed
  sizes is invalid *even when nothing is shared* — so a Combined Import Cost is an upper bound
  and must never be presented as a size. The sole exception is summing across genuine artifact
  boundaries; see [ADR-0005](0005-a-runtime-is-an-artifact-boundary.md).
- "Adding `zod` here costs nothing, it's already in your bundle" — Marginal Cost — is the
  natural next question and this product cannot answer it. Answering it means building the
  union model this ADR declines. That is a new product, to be decided on its own merits, not
  smuggled in as a bug fix.
