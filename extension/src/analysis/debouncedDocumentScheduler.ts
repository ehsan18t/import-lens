/**
 * Per-document debounce shared by the document and package.json analysis
 * controllers. Keyed by an opaque string (the document URI), it keeps at most
 * one pending run per key: scheduling a key that is already pending cancels and
 * replaces it, and a run is forgotten as soon as it fires so the map never
 * accumulates dead timer handles. Deliberately vscode-free so it unit-tests
 * directly; callers pass `document.uri.toString()` and `config.debounceMs`.
 */
export class DebouncedDocumentScheduler {
  readonly #timers = new Map<string, NodeJS.Timeout>();

  schedule(key: string, delayMs: number, run: () => void): void {
    this.cancel(key);
    this.#timers.set(
      key,
      setTimeout(() => {
        this.#timers.delete(key);
        run();
      }, delayMs),
    );
  }

  cancel(key: string): void {
    const timer = this.#timers.get(key);

    if (timer) {
      clearTimeout(timer);
      this.#timers.delete(key);
    }
  }

  dispose(): void {
    for (const timer of this.#timers.values()) {
      clearTimeout(timer);
    }

    this.#timers.clear();
  }
}
