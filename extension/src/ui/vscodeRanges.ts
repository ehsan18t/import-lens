import * as vscode from "vscode";
import type { SourceEdit } from "../analysis/rangeTracking.js";
import type { SourceRange } from "../ipc/protocol.js";

export const rangeFromSourceRange = (range: SourceRange): vscode.Range =>
  new vscode.Range(range.start.line, range.start.character, range.end.line, range.end.character);

export const sourceEditFromChange = (
  change: vscode.TextDocumentContentChangeEvent,
): SourceEdit => ({
  range: {
    start: { line: change.range.start.line, character: change.range.start.character },
    end: { line: change.range.end.line, character: change.range.end.character },
  },
  text: change.text,
});
