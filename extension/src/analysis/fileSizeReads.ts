/**
 * How many `file_size_document` reads one analysis generation may issue, and when.
 *
 * The answer is **one at a time, and one re-read at most** — and it is a policy rather than a
 * scheduling detail, because a second read for the same generation is not merely wasteful. The
 * daemon **supersedes** the older request and answers it `error: "superseded by a newer request"`,
 * and an errored size read *withdraws* the File Cost from the store and blanks the status bar for a
 * round trip — on exactly the cold document the second read exists to serve.
 */
export type FileSizeReadPhase = "unread" | "reading" | "read";

export interface FileSizeReadState {
  /** `unread` — the analysis has not issued its own size read yet; `reading` — one is in flight. */
  readonly phase: FileSizeReadPhase;
  /** A settle landed with a read in flight, so one more read is owed once that flight comes home. */
  readonly refetchOwed: boolean;
  /** One re-read per analysis: a re-read is itself a size request, which can serve stale and push a
   * refresh of its own, and a re-read armed by every push is a loop. */
  readonly refetched: boolean;
}

/** A decision, and the state that produced it. `read` is "issue a `file_size_document` now". */
export interface FileSizeReadStep {
  readonly state: FileSizeReadState;
  readonly read: boolean;
}

export const initialFileSizeReadState: FileSizeReadState = {
  phase: "unread",
  refetchOwed: false,
  refetched: false,
};

/** A `file_size_document` for this generation has gone out. */
export const sizeReadStarted = (state: FileSizeReadState): FileSizeReadState => ({
  ...state,
  phase: "reading",
});

/**
 * It came back — success, `error`, or a thrown round trip; all three end the flight.
 *
 * If a settle landed while it was in the air, the number it brought home may still be a floor: it
 * was asked for before the document's last import was measured. That is the re-read this owes, and
 * now is when nothing is in flight for it to supersede.
 */
export const sizeReadFinished = (state: FileSizeReadState): FileSizeReadStep => {
  const settled: FileSizeReadState = { ...state, phase: "read", refetchOwed: false };

  if (!state.refetchOwed || state.refetched) {
    return { state: settled, read: false };
  }

  return { state: { ...settled, refetched: true }, read: true };
};

/**
 * Every import in the document has landed, so the File Cost read the analysis made while it was
 * COLD — when an unmeasured import made the file's total a floor, and a floor may not be judged
 * against a budget at all — can now be re-taken against a document that is fully measured.
 *
 * The three answers, and only one of them is a request:
 *
 * - **`unread`** — the analysis has not issued its size read yet, and it is about to. Its read IS
 *   the settled read. Issuing one here would be the SECOND `file_size_document` for one generation,
 *   and on a cold document whose imports all stream in before the git-diff await resolves, that is
 *   exactly what happened: the daemon superseded the first, answered it `error: "superseded by a
 *   newer request"`, and the status bar blanked for a round trip.
 * - **`reading`** — a read is in the air. Wait for it rather than superseding it; `sizeReadFinished`
 *   issues the one that is owed.
 * - **`read`** — nothing is in flight and the document is settled. Read it, once.
 */
export const documentSettled = (state: FileSizeReadState): FileSizeReadStep => {
  if (state.refetched) {
    return { state, read: false };
  }

  if (state.phase === "unread") {
    return { state, read: false };
  }

  if (state.phase === "reading") {
    return { state: { ...state, refetchOwed: true }, read: false };
  }

  return { state: { ...state, refetched: true }, read: true };
};
