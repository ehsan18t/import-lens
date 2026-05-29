import type { SourcePosition, SourceRange } from "./types.js";

export const positionAt = (source: string, offset: number): SourcePosition => {
  const safeOffset = Math.max(0, Math.min(offset, source.length));
  const before = source.slice(0, safeOffset);
  const lines = before.split(/\r\n|\r|\n/u);
  return {
    line: lines.length - 1,
    character: lines[lines.length - 1]?.length ?? 0,
  };
};

export const rangeFromOffsets = (source: string, start: number, end: number): SourceRange => ({
  start: positionAt(source, start),
  end: positionAt(source, end),
});

