export const compilerStackConfig = {
  currentRolldownVersion: "1.1.5",
  currentOxcVersion: "0.139.0",
  currentResolverVersion: "11.23.0",
  // The glob matcher Rolldown itself reads `sideEffects` with
  // (`rolldown_common`, `rolldown_utils`, and oxc_resolver). The daemon matches
  // the entry against the declared patterns to decide the Side-Effectful badge,
  // and the ONLY thing that makes that answer right is that it agrees with the
  // bundler that owns retention -- so the two must be the SAME matcher at the
  // SAME version, or the agreement breaks silently. It is not chosen: the
  // updater reads it out of the version Cargo resolves for rolldown's own graph.
  currentGlobMatcherVersion: "1.0.1",
  globMatcherCrate: "fast-glob",
  rolldownCrate: "rolldown",
  // Rolldown monorepo siblings the adapter depends on directly (they carry
  // the public output/diagnostic types the rolldown root does not
  // re-export). Published at the same monorepo version as rolldown and
  // pinned in lockstep; the updater's probe rejects any release where the
  // shared-version invariant does not hold.
  rolldownSupportCrates: ["rolldown_common", "rolldown_error"],
  oxcCrates: [
    "oxc_allocator",
    "oxc_codegen",
    "oxc_minifier",
    "oxc_parser",
    "oxc_semantic",
    "oxc_span",
    "oxc_syntax",
  ],
};
