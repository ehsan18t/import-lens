export class LogDedupe {
  readonly #seen = new Set<string>();

  once(key: string, emit: () => void): void {
    if (this.#seen.has(key)) {
      return;
    }

    this.#seen.add(key);
    emit();
  }

  clear(): void {
    this.#seen.clear();
  }
}
