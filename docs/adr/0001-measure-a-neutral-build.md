# Measure a neutral build, not a production or development one

Rolldown is configured with `Platform::Neutral` and no `process.env.NODE_ENV` define
(`daemon/src/engine/adapter.rs:111-120`), so a package's development-only branches are
counted in its Import Cost. This looks like an oversight and has been reported as one; it
is deliberate. Our accuracy oracle is **esbuild**, which likewise injects no `NODE_ENV`
define by default, so a neutral measurement is what keeps the two comparable — injecting
production would improve nothing while moving our numbers away from the only ground truth
we have. A neutral platform also avoids Rolldown deriving `Platform::Browser` from the ESM
format and appending browser resolution behaviour we did not ask for.

## Considered options

- **Measure the production build.** Inject `NODE_ENV=production` and production export
  conditions. Rejected: it breaks oracle-parity, invalidates every accuracy baseline and
  matrix expectation, and buys accuracy we cannot verify.
- **A user-selected mode** (`neutral | production | development`). Considered and rejected
  as not worth its cost: the mode would have to enter the cache key, plumb through to both
  the define and the export conditions, and be explained to users — for a number that is
  already defensible.
- **Read the project's real bundler config** (conditions, defines, ES target, browserslist).
  A different and much larger product: it needs config discovery and per-project state. Not
  ruled out forever; ruled out as a rider on anything else.

## Consequences

A package whose development branches are large — React is the standard example — is reported
heavier than what the user will ship. That is a known and accepted limit of an Import Cost:
it prices the source as published, not as any one project will configure it.
