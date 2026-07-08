import { AnalysisFreshnessTracker } from "../analysis/freshness.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import { AnalyzedContentTracker } from "./analyzedContentTracker.js";

// Pure coordinator for package.json analysis request freshness and same-text
// coalescing. Kept free of vscode imports so node:test can exercise the races
// directly.
export class PackageJsonRequestLifecycle {
  readonly #freshness = new AnalysisFreshnessTracker();
  readonly #content = new AnalyzedContentTracker();

  shouldSkipUnchanged(key: string, text: string): boolean {
    return this.#content.isUnchanged(key, text);
  }

  begin(key: string, text: string): number {
    const requestId = this.#freshness.begin(key, nextIpcRequestId());
    this.#content.record(key, text);
    return requestId;
  }

  isCurrent(key: string, requestId: number): boolean {
    return this.#freshness.isCurrent(key, requestId);
  }

  fail(key: string): void {
    this.#content.forget(key);
  }

  forget(key: string): void {
    this.#freshness.forget(key);
    this.#content.forget(key);
  }

  supersedeAll(): void {
    this.#freshness.clear();
    this.#content.forgetAll();
  }
}
