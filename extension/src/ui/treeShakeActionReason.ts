import type { ImportResult } from "../ipc/protocol.js";
import { measuredSizes } from "./format.js";

export const treeShakeActionReason = (result: ImportResult): string | null => {
  // "Is there a size?", never "is there an error?" (ADR-0006, invariant 2). Every field this reads
  // below — `is_cjs`, `side_effects`, `truly_treeshakeable` — is decided by a build, so a result no
  // build produced has nothing to say about tree-shaking. `error` was only ever a proxy for that,
  // and it is the wrong one: a Loading result has no size and no error either.
  if (!measuredSizes(result)) {
    return null;
  }

  if (result.is_cjs) {
    return "CommonJS import may block precise tree-shaking.";
  }

  if (result.side_effects) {
    return "Package side effects require conservative sizing.";
  }

  if (!result.truly_treeshakeable) {
    return "Import is not tree-shakeable by the current analysis.";
  }

  return null;
};
