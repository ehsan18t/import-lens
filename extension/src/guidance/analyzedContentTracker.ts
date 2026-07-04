/**
 * Records, per document key, the exact text of the last successful package.json
 * analysis so passive re-triggers (tab focus, re-open) can skip redundant work.
 * Explicit re-analysis paths (config change, daemon restart, cache clear,
 * node_modules watcher) call `forget` first to force a fresh run.
 */
export class AnalyzedContentTracker {
  readonly #content = new Map<string, string>();

  isUnchanged(key: string, text: string): boolean {
    return this.#content.get(key) === text;
  }

  record(key: string, text: string): void {
    this.#content.set(key, text);
  }

  forget(key: string): void {
    this.#content.delete(key);
  }

  /**
   * Drops every recorded document. Explicit refreshes call this so that even
   * open-but-not-visible package.json tabs re-analyze when next focused, rather
   * than being wrongly skipped by the unchanged-content guard.
   */
  forgetAll(): void {
    this.#content.clear();
  }
}
