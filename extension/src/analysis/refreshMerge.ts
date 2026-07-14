import type { ImportResult, ImportRuntime, RefreshedImportIdentity } from "../ipc/protocol.js";
import type { ImportAnalysisState } from "./state.js";

export interface RefreshMergeOutcome {
  next: ImportAnalysisState[];
  changed: boolean;
}

export interface RefreshMergeOptions {
  /**
   * Per-result import identity, index-aligned with `results` (the SWR push carries
   * it). Present -> results are keyed by full identity (specifier + kind + named +
   * runtime) so two imports of the same package differing only by import kind /
   * named exports / runtime each receive their OWN refreshed size. Absent or
   * length-mismatched (older daemon) -> specifier-only keying, preserving legacy
   * behavior.
   */
  identities?: readonly RefreshedImportIdentity[];
  /**
   * Whether this refresh batch is still current for the document, per the same
   * `AnalysisFreshnessTracker.isCurrent` gate that guards `updateFileSize`. `false`
   * means a newer analysis has superseded it (the user edited after it was
   * computed) -> the batch is dropped rather than overwriting current states.
   * Undefined (no correlatable generation) -> applied, preserving legacy behavior.
   */
  isCurrent?: boolean;
}

// A stable, order-independent key for one import. specifier alone is NOT unique
// (same-specifier variants differ by kind / named / runtime), so the full identity
// keys the merge. NUL/SOH separators keep field boundaries unambiguous (a specifier
// or export name cannot contain them), and `named` is sorted so a differing source
// order still yields the same key.
//
// The runtime is part of the key because it is part of the import. An Astro document can import the
// same package, with the same kind and the same named exports, from its frontmatter (server) and
// from a client <script>; the two resolve dependencies under materially different conditions, so
// they have two different sizes, and each runtime ships its own artifact (ADR-0005). Without it the
// two variants collide on ONE key: the map keeps a single result, both states match it, and the
// client collapses two rows into one — in the very document shape the runtime split exists for.
const identityKey = (
  specifier: string,
  importKind: string,
  named: readonly string[],
  runtime: ImportRuntime,
): string =>
  `${specifier}\u0000${importKind}\u0000${runtime}\u0000${[...named].sort().join("\u0001")}`;

/**
 * Pure merge of pushed import results into a document's states, matched by a stable
 * per-import identity. Two kinds of push arrive here and both are merged the same way:
 * a background stale-while-revalidate refresh, and an import the daemon answered
 * `loading` because its engine build had not run when the response went out.
 *
 * Order is preserved; unmatched states pass through untouched. Insights are dropped on
 * a match: they were computed against the replaced result, so carrying them over would
 * caption a fresh size with the old value's commentary. The caller recomputes them.
 *
 * A superseded batch (`options.isCurrent === false`) is dropped wholesale so a refresh
 * computed against an old document state cannot overwrite the current (post-edit)
 * states — mirroring `listener.ts`'s `freshness.isCurrent` gate.
 *
 * **An errored result may fill a state that has no result, and may never replace one
 * that has.** The two pushes need opposite things from an error, and the state says
 * which: a revalidation that failed must not throw away a good (if stale) size the user
 * is looking at, while a `loading` import whose build genuinely failed has nothing to
 * protect and would otherwise sit at "Calculating..." for ever — the failure IS its
 * answer.
 *
 * Kept vscode-free (the store wraps it) so the merge semantics are unit-testable
 * under the repo's plain `node --test` harness.
 */
export const mergeRefreshedResults = (
  existing: readonly ImportAnalysisState[],
  results: readonly ImportResult[],
  options?: RefreshMergeOptions,
): RefreshMergeOutcome => {
  // Supersession guard: a batch the daemon computed for an analysis that a newer
  // one has since replaced must not clobber the current states.
  if (options?.isCurrent === false) {
    return { next: [...existing], changed: false };
  }

  const { identities } = options ?? {};
  // Only trust identities when they are index-aligned with results; otherwise fall
  // back to specifier keying so an older/partial daemon push still merges (rather
  // than silently matching nothing). Same-specifier collisions are impossible once
  // a valid identity set is present.
  const useIdentity = identities !== undefined && identities.length === results.length;

  const byKey = new Map<string, ImportResult>(
    results.map((result, index) => {
      const key = useIdentity
        ? identityKey(
            identities[index].specifier,
            identities[index].import_kind,
            identities[index].named,
            identities[index].runtime,
          )
        : result.specifier;
      return [key, result];
    }),
  );

  let changed = false;
  const next = existing.map((state) => {
    const key = useIdentity
      ? identityKey(
          state.detected.specifier,
          state.detected.importKind,
          state.detected.named,
          state.detected.runtime,
        )
      : state.detected.specifier;
    const refreshed = byKey.get(key);

    if (!refreshed) {
      return state;
    }

    // Never downgrade a measurement to a failure; an import that has none takes what lands.
    if (refreshed.error !== null && state.result !== undefined) {
      return state;
    }

    changed = true;
    return { ...state, status: "ready" as const, result: refreshed, insights: undefined };
  });

  return { next, changed };
};
