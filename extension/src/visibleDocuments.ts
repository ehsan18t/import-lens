import type * as vscode from "vscode";

type EditorWindow = Pick<typeof vscode.window, "activeTextEditor" | "visibleTextEditors">;

/**
 * Whether an editor pane shows `document` right now. VS Code also opens documents it never shows
 * (peek and go-to-definition previews, diff views, files other extensions read), and none of those
 * is worth a build. The active editor counts even before the visible-editor list includes it: the
 * two change events have no guaranteed order.
 */
export const isShownDocument = (document: vscode.TextDocument, window: EditorWindow): boolean =>
  window.activeTextEditor?.document === document ||
  window.visibleTextEditors.some((editor) => editor.document === document);

/** The documents in `documents` whose URI `previous` does not hold, each once. */
export const newlyVisibleDocuments = (
  documents: readonly vscode.TextDocument[],
  previous: ReadonlySet<string>,
): vscode.TextDocument[] => {
  const seen = new Set<string>();
  return documents.filter((document) => {
    const key = document.uri.toString();
    if (previous.has(key) || seen.has(key)) {
      return false;
    }
    seen.add(key);
    return true;
  });
};
