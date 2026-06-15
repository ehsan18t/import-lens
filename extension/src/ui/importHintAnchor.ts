import type { DetectedImport, SourcePosition } from "../imports/types.js";

export interface ImportHintAnchorDocument {
  readonly lineCount: number;
  lineAt(line: number): { readonly text: string };
}

export const importHintAnchorPosition = (
  document: ImportHintAnchorDocument,
  detected: DetectedImport,
): SourcePosition => {
  const lineNumber = Math.min(detected.statementRange.end.line, document.lineCount - 1);
  const line = document.lineAt(lineNumber);

  return {
    line: lineNumber,
    character: Math.min(detected.statementRange.end.character, line.text.length),
  };
};
