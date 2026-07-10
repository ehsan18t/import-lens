// Pure, vscode-free status-bar rendering so the label logic is unit-testable
// under `node --test` (which has no `vscode` module). The StatusBarController in
// `statusbar.ts` consumes these.

export type StatusBarState =
  | { kind: "ready" }
  | { kind: "computing" }
  | { kind: "unavailable" }
  | { kind: "size"; label: string };

export const statusBarText = (state: StatusBarState): string => {
  switch (state.kind) {
    case "size":
      return `IL: ${state.label}`;
    case "computing":
      return "IL: Computing…";
    case "unavailable":
      return "IL: Unavailable";
    case "ready":
      return "IL: Ready";
  }
};

export const statusBarTooltip = (state: StatusBarState): string => {
  switch (state.kind) {
    case "size":
      return `Import Lens: Current file bundle size (${state.label})`;
    case "computing":
      return "Import Lens: Computing current file size";
    case "unavailable":
      return "Import Lens: Daemon unavailable";
    case "ready":
      return "Import Lens: Ready";
  }
};
