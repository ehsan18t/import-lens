import type { ImportResult } from "../ipc/protocol.js";

export const treeShakeActionReason = (result: ImportResult): string | null => {
  if (result.error) {
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
