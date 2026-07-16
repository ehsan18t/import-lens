// Pure, vscode-free status-bar rendering so the label logic is unit-testable
// under `node --test` (which has no `vscode` module). The StatusBarController in
// `statusbar.ts` consumes these.

import {
  type FileCostQuality,
  fileCostBecause,
  fileCostQuantityName,
  isFileCost,
} from "../analysis/fileCostQuality.js";
import { type CompressionFormat, formatBytes, labelForCompression } from "./format.js";

/**
 * The size state carries **the number and what the number IS** — not a string somebody already
 * decided the words for.
 *
 * It used to be `{ label: string }`, and `listener.ts` baked a `~` into that label for `incomplete`
 * and `degraded` alike. The quality was **erased at the extension boundary**: the daemon knew
 * exactly what it had sent, and by the time the tooltip ran there was nothing left to read, so it
 * guessed — and named an un-deduplicated per-import sum a "File Cost, built as one bundle", which is
 * the one thing that provably did not happen.
 */
export type StatusBarState =
  | { kind: "ready" }
  | { kind: "computing" }
  | { kind: "unavailable" }
  | { kind: "size"; bytes: number; compression: CompressionFormat; quality: FileCostQuality };

/** `~` marks a figure that is not the file's size (FR-031's existing mark for an inexact number). */
const sizeLabel = (state: { bytes: number; compression: CompressionFormat }, exact: boolean) =>
  `${exact ? "" : "~"}${formatBytes(state.bytes)} ${labelForCompression(state.compression)}`;

export const statusBarText = (state: StatusBarState): string => {
  switch (state.kind) {
    case "size":
      return `IL: ${sizeLabel(state, isFileCost(state.quality))}`;
    case "computing":
      return "IL: Computing…";
    case "unavailable":
      return "IL: Unavailable";
    case "ready":
      return "IL: Ready";
  }
};

/**
 * The status bar's words, **derived from the quantity the daemon handed over** — never guessed.
 *
 * When the number is a File Cost it says so: the daemon's ONE combined build over this file's
 * imports, in which a module two of them reach is counted once, priced against an otherwise-empty
 * app (ADR-0004). It is not a bundle size, and it called itself one ("Current file bundle size") for
 * the whole life of the product.
 *
 * When it is **not** a File Cost, it says which number it is, why, and that **no budget was judged
 * from it** — which is exactly what `importlens check` has printed for the same number, on the same
 * run, all along. Two surfaces, one number, and the always-on-screen one was the one that lied.
 */
export const statusBarTooltip = (state: StatusBarState): string => {
  switch (state.kind) {
    case "size": {
      const exact = isFileCost(state.quality);
      const verdict = exact ? "" : " Budget not evaluated.";
      return `Import Lens: ${fileCostQuantityName(state.quality)} (${sizeLabel(state, exact)}) — ${fileCostBecause(state.quality)}.${verdict}`;
    }
    case "computing":
      return "Import Lens: Computing current file size";
    case "unavailable":
      return "Import Lens: Daemon unavailable";
    case "ready":
      return "Import Lens: Ready";
  }
};
