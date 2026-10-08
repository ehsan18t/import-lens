import type { DetectedImport, SourcePosition, SourceRange } from "../ipc/protocol.js";

/**
 * One text change, as `vscode.TextDocumentContentChangeEvent` reports it: the range it replaced (in
 * the text before the change) and the text it put there. The changes of one event apply in the
 * order given, each against the text the previous one left.
 */
export interface SourceEdit {
  readonly range: SourceRange;
  readonly text: string;
}

/**
 * Where a position lands after an edit.
 *
 * `stickiness` decides the ambiguous case, an edit that starts exactly at the position: the start
 * of a range moves along with text typed in front of it, the end of a range stays where it is
 * (typing after a statement's `;`, or over a comment that follows it, does not grow the statement).
 * A position inside the replaced text moves to the replacement's start (a range start) or end (a
 * range end).
 */
export const shiftPosition = (
  position: SourcePosition,
  edit: SourceEdit,
  stickiness: "start" | "end",
): SourcePosition => {
  const { start, end } = edit.range;
  const isInsertion = comparePositions(start, end) === 0;
  const atOrAfterEnd =
    stickiness === "start" || !isInsertion
      ? comparePositions(position, end) >= 0
      : comparePositions(position, end) > 0;

  if (atOrAfterEnd) {
    return positionAfterEdit(position, edit);
  }

  const fromStart = comparePositions(position, start);

  if (fromStart < 0 || (fromStart === 0 && stickiness === "end")) {
    return position;
  }

  return stickiness === "start" ? start : insertedTextEnd(edit);
};

/**
 * A range after an edit, or `null` when the edit replaced all of it (the import was deleted or
 * retyped wholesale, and only a re-analysis knows what stands there now).
 */
export const shiftRange = (range: SourceRange, edit: SourceEdit): SourceRange | null => {
  const covers =
    comparePositions(edit.range.start, range.start) <= 0 &&
    comparePositions(range.end, edit.range.end) <= 0 &&
    comparePositions(edit.range.start, edit.range.end) < 0;

  if (covers) {
    return null;
  }

  return {
    start: shiftPosition(range.start, edit, "start"),
    end: shiftPosition(range.end, edit, "end"),
  };
};

/** A range carried through edits in order, or `null` once one of them replaced all of it. */
export const shiftRangeThrough = (
  range: SourceRange,
  edits: readonly SourceEdit[],
): SourceRange | null => {
  let shifted: SourceRange | null = range;

  for (const edit of edits) {
    if (!shifted) {
      return null;
    }

    shifted = shiftRange(shifted, edit);
  }

  return shifted;
};

/** Whether a range covers no text: what a range an edit replaced collapses to. */
export const isEmptyRange = (range: SourceRange): boolean =>
  comparePositions(range.start, range.end) === 0;

/** A detected import's positions after the edits, or `null` once its statement is gone. */
export const shiftDetectedImport = (
  detected: DetectedImport,
  edits: readonly SourceEdit[],
): DetectedImport | null => {
  let shifted = detected;

  for (const edit of edits) {
    const statementRange = shiftRange(shifted.statementRange, edit);

    if (!statementRange) {
      return null;
    }

    shifted = {
      ...shifted,
      line: statementRange.start.line,
      statementRange,
      specifierRange: shiftRange(shifted.specifierRange, edit) ?? {
        start: statementRange.start,
        end: statementRange.start,
      },
      quoteEnd: shiftPosition(shifted.quoteEnd, edit, "end"),
    };
  }

  return shifted;
};

/** Line numbers (the git working-tree diff's) renumbered through the edits; lines an edit removed drop out. */
export const shiftLines = (
  lines: ReadonlySet<number>,
  edits: readonly SourceEdit[],
): Set<number> => {
  let current = [...lines];

  for (const edit of edits) {
    current = current.flatMap((line) => {
      const range = shiftRange(
        { start: { line, character: 0 }, end: { line, character: Number.MAX_SAFE_INTEGER } },
        edit,
      );
      return range ? [range.start.line] : [];
    });
  }

  return new Set(current);
};

/** Bound on the edits kept per document between the text an analysis read and its response. */
const maxLoggedEditsPerDocument = 1000;

/**
 * The edits each document has had since a given version, so states the daemon computed for an
 * older text are laid onto the text on screen before they are stored. An analysis reads the text
 * at one version and its response lands several keystrokes later; stored as it comes, every range
 * in it would point at where the import was, not where it is.
 */
export class DocumentEditLog {
  readonly #entries = new Map<string, { version: number; edits: readonly SourceEdit[] }[]>();
  // The newest version whose edits were dropped by the bound, per document: an analysis that read
  // an older text can no longer be laid onto the current one.
  readonly #droppedThrough = new Map<string, number>();

  record(key: string, version: number, edits: readonly SourceEdit[]): void {
    const entries = this.#entries.get(key) ?? [];
    entries.push({ version, edits });

    while (entries.length > maxLoggedEditsPerDocument) {
      const dropped = entries.shift();

      if (dropped) {
        this.#droppedThrough.set(key, dropped.version);
      }
    }

    this.#entries.set(key, entries);
  }

  /** The edits made after `version`, in order, or `null` when some of them are no longer known. */
  since(key: string, version: number): SourceEdit[] | null {
    if ((this.#droppedThrough.get(key) ?? Number.NEGATIVE_INFINITY) > version) {
      return null;
    }

    return (this.#entries.get(key) ?? [])
      .filter((entry) => entry.version > version)
      .flatMap((entry) => entry.edits);
  }

  /** Drop what no analysis that read `version` or later can need. */
  prune(key: string, version: number): void {
    const entries = this.#entries.get(key);

    if (entries) {
      this.#entries.set(
        key,
        entries.filter((entry) => entry.version > version),
      );
    }
  }

  forget(key: string): void {
    this.#entries.delete(key);
    this.#droppedThrough.delete(key);
  }

  clear(): void {
    this.#entries.clear();
    this.#droppedThrough.clear();
  }
}

const comparePositions = (left: SourcePosition, right: SourcePosition): number =>
  left.line === right.line ? left.character - right.character : left.line - right.line;

const insertedTextEnd = (edit: SourceEdit): SourcePosition => {
  const lines = edit.text.split("\n");
  const lastLine = lines.at(-1) ?? "";

  return lines.length === 1
    ? { line: edit.range.start.line, character: edit.range.start.character + lastLine.length }
    : { line: edit.range.start.line + lines.length - 1, character: lastLine.length };
};

const positionAfterEdit = (position: SourcePosition, edit: SourceEdit): SourcePosition => {
  const insertedEnd = insertedTextEnd(edit);
  const { end } = edit.range;

  if (position.line === end.line) {
    return {
      line: insertedEnd.line,
      character: insertedEnd.character + (position.character - end.character),
    };
  }

  return { line: position.line + (insertedEnd.line - end.line), character: position.character };
};
