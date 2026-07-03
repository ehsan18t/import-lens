import * as vscode from "vscode";
import type { SourceRange } from "../ipc/protocol.js";

export const rangeFromSourceRange = (range: SourceRange): vscode.Range =>
  new vscode.Range(
    range.start.line,
    range.start.character,
    range.end.line,
    range.end.character,
  );
