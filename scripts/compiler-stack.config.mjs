export const compilerStackConfig = {
  currentRolldownVersion: "1.1.5",
  currentOxcVersion: "0.139.0",
  currentResolverVersion: "11.23.0",
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
