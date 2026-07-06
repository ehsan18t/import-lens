import type { ImportResult, RefreshedImportIdentity } from "../ipc/protocol.js";
import type { ImportAnalysisState } from "./state.js";

export interface RefreshMergeOutcome {
  next: ImportAnalysisState[];
  changed: boolean;
}

export interface RefreshMergeOptions {
  /**
   * Per-result import identity, index-aligned with `results` (the SWR push carries
   * it). Present -> results are keyed by full identity (specifier + kind + named)
   * so two imports of the same package differing only by import kind / named
   * exports each receive their OWN refreshed size. Absent or length-mismatched
   * (older daemon) -> specifier-only keying, preserving legacy behavior.
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
// (same-specifier variants differ by kind / named), so the full identity keys the
// merge. NUL/SOH separators keep field boundaries unambiguous (a specifier or
// export name cannot contain them), and `named` is sorted so a differing source
// order still yields the same key.
const identityKey = (specifier: string, importKind: string, named: readonly string[]): string =>
  `${specifier}\u0000${importKind}\u0000${[...named].sort().join("\u0001")}`;

/**
 * Pure merge of background-refreshed sizes (the daemon's stale-while-revalidate
 * push) into a document's states, matched by a stable per-import identity. Order
 * is preserved; unmatched states pass through untouched. Insights are dropped on a
 * match: they were computed against the replaced (stale) result, so carrying them
 * over would caption the fresh size with the old value's commentary — the next
 * full analysis recomputes them.
 *
 * A superseded batch (`options.isCurrent === false`) is dropped wholesale so a
 * refresh computed against an old document state cannot overwrite the current
 * (post-edit) states — mirroring `listener.ts`'s `freshness.isCurrent` gate.
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
  const cacheableResults = results
    .map((result, index) => ({ result, index }))
    .filter(({ result }) => result.error === null);

  const byKey = new Map<string, ImportResult>(
    cacheableResults.map(({ result, index }) => {
      const key = useIdentity
        ? identityKey(
            identities[index].specifier,
            identities[index].import_kind,
            identities[index].named,
          )
        : result.specifier;
      return [key, result];
    }),
  );

  let changed = false;
  const next = existing.map((state) => {
    const key = useIdentity
      ? identityKey(state.detected.specifier, state.detected.importKind, state.detected.named)
      : state.detected.specifier;
    const refreshed = byKey.get(key);

    if (!refreshed) {
      return state;
    }

    changed = true;
    return { ...state, status: "ready" as const, result: refreshed, insights: undefined };
  });

  return { next, changed };
};
