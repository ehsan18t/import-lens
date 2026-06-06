export class AnalysisFreshnessTracker {
  #nextRequestId = 0;
  readonly #latestRequestIds = new Map<string, number>();

  begin(documentKey: string, requestId: number = this.#nextRequestId + 1): number {
    this.#nextRequestId = Math.max(this.#nextRequestId, requestId);
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
