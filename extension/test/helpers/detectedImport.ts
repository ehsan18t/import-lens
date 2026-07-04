import type { DetectedImport, SourceRange } from "../../src/ipc/protocol.js";

export const sourceRange = (
  line: number,
  startCharacter: number,
  endCharacter: number,
): SourceRange => ({
  start: { line, character: startCharacter },
  end: { line, character: endCharacter },
});

export const detectedImport = (overrides: Partial<DetectedImport> = {}): DetectedImport => {
  const line = overrides.line ?? 0;
  const statementRange = overrides.statementRange ?? sourceRange(line, 0, 24);
  const specifierRange = overrides.specifierRange ?? sourceRange(line, 8, 18);

  const base: DetectedImport = {
    specifier: "test-lib",
    packageName: "test-lib",
    named: [],
    importKind: "namespace",
    syntax: "static",
    runtime: "component",
    line,
    quoteEnd: { line: specifierRange.end.line, character: specifierRange.end.character },
    specifierRange,
    statementRange,
  };

  return {
    ...base,
    ...overrides,
    quoteEnd: overrides.quoteEnd ?? base.quoteEnd,
    specifierRange,
    statementRange,
  };
};
