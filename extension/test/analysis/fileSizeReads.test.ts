import assert from "node:assert/strict";
import test from "node:test";
import {
  documentSettled,
  type FileSizeReadState,
  initialFileSizeReadState,
  sizeReadFinished,
  sizeReadStarted,
} from "../../src/analysis/fileSizeReads.js";

/**
 * A stand-in for `DocumentAnalysisController`'s half of the protocol: it holds the state, counts the
 * reads the policy asks for, and calls nothing the listener does not call.
 */
class SizeReads {
  #state: FileSizeReadState = initialFileSizeReadState;
  issued = 0;
  inFlight = 0;

  /** `analyze()` and `refetchFileSizeWhenSettled()` both go through here. */
  read(): void {
    this.#state = sizeReadStarted(this.#state);
    this.issued += 1;
    this.inFlight += 1;
  }

  /** The daemon answered. */
  answered(): void {
    this.inFlight -= 1;
    const step = sizeReadFinished(this.#state);
    this.#state = step.state;

    if (step.read) {
      this.read();
    }
  }

  /** A streamed push landed the document's last import. */
  settled(): void {
    const step = documentSettled(this.#state);
    this.#state = step.state;

    if (step.read) {
      this.read();
    }
  }
}

/**
 * The regression `4fabfd9` introduced, on the document it was written for.
 *
 * A cold document's imports ALL arrive by push, and on a warm daemon they can arrive before
 * `analyze()`'s `git diff` await resolves. The push settles the document and arms the re-read; the
 * analysis then issues its own size read for the SAME generation. Two `file_size_document` requests,
 * one generation: the daemon supersedes the first, answers it `error: "superseded by a newer
 * request"`, the extension withdraws the File Cost it never had — and the status bar blanks for a
 * round trip.
 *
 * The re-read's INTENT is right (the analysis' read ran while the file's total was still a floor,
 * and a floor may not be judged against a budget). The double request is the bug: the analysis has
 * not read yet, so its read is the settled read.
 */
test("a settle before the analysis' own size read issues no read of its own", () => {
  const reads = new SizeReads();

  // Every import streams in while `analyze()` is still awaiting its git diff.
  reads.settled();
  // …and then `analyze()` reads the file's size, as it always does.
  reads.read();
  reads.answered();

  assert.equal(reads.issued, 1, "one generation, ONE file_size_document");
});

/**
 * The same rule from the other side: a settle that lands while a read is IN FLIGHT must not
 * supersede it. The read in flight was issued before the last import landed, so its number may still
 * be a floor and one more read is genuinely owed — after the one in flight comes home, not on top
 * of it.
 */
test("a settle during a read waits for it, then re-reads exactly once", () => {
  const reads = new SizeReads();

  reads.read();
  reads.settled();

  assert.equal(reads.inFlight, 1, "the in-flight read is never superseded by a second one");
  assert.equal(reads.issued, 1);

  reads.answered();

  assert.equal(reads.issued, 2, "the re-read the settle owed is issued once the flight lands");
  reads.answered();
  assert.equal(reads.issued, 2);
});

/** The cold document the re-read exists for: it settles after the analysis' read came home. */
test("a settle after the analysis' read re-reads the file once", () => {
  const reads = new SizeReads();

  reads.read();
  reads.answered();
  reads.settled();

  assert.equal(reads.issued, 2, "the analysis read a floor; the settled document is re-read");

  reads.answered();
  reads.settled();
  reads.settled();

  assert.equal(reads.issued, 2, "one re-read per analysis — a re-read per push is a loop");
});
