export class AnalysisFreshnessTracker {
  #nextRequestId = 0;
  readonly #latestRequestIds = new Map<string, number>();

  begin(documentKey: string): number {
    const requestId = ++this.#nextRequestId;
    this.#latestRequestIds.set(documentKey, requestId);
    return requestId;
  }

  isCurrent(documentKey: string, requestId: number): boolean {
    return this.#latestRequestIds.get(documentKey) === requestId;
  }

  forget(documentKey: string): void {
    this.#latestRequestIds.delete(documentKey);
  }

  clear(): void {
    this.#latestRequestIds.clear();
  }
}
