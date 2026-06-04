import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";

const RECYCLE_FILE_NAME = "importlens-recycles.json";
const RECYCLE_WINDOW_MS = 10 * 60 * 1000;
const CLEAN_SESSION_MS = 30 * 60 * 1000;
const MAX_RECYCLES_IN_WINDOW = 5;

interface RecycleFile {
  recycles?: unknown;
}

export class RecycleGuard {
  readonly #storagePath: string;
  #cachedRecycleTimes: number[] | null = null;
  #loadPromise: Promise<number[]> | null = null;
  #pendingMutation: Promise<void> = Promise.resolve();

  constructor(storagePath: string) {
    this.#storagePath = storagePath;
  }

  async shouldEnterDegradedMode(now: number = Date.now()): Promise<boolean> {
    const recent = this.#recentRecycleTimes(await this.readRecycleTimes(), now);
    return recent.length > MAX_RECYCLES_IN_WINDOW;
  }

  async recordRecycle(now: number = Date.now()): Promise<void> {
    await this.#mutateRecycleTimes((recycleTimes) => [
      ...this.#recentRecycleTimes(recycleTimes, now),
      now,
    ]);
  }

  async resetAfterCleanSession(now: number = Date.now()): Promise<void> {
    await this.#mutateRecycleTimes((recycleTimes) => {
      const hasRecentRecycle = recycleTimes.some((timestamp) => now - timestamp <= CLEAN_SESSION_MS);

      return hasRecentRecycle ? recycleTimes : [];
    });
  }

  async readRecycleTimes(): Promise<number[]> {
    await this.#pendingMutation;
    return [...(await this.#loadRecycleTimes())];
  }

  async recordRecycleTimes(recycleTimes: readonly number[]): Promise<void> {
    await this.#replaceRecycleTimes(recycleTimes);
  }

  async #loadRecycleTimes(): Promise<number[]> {
    if (this.#cachedRecycleTimes) {
      return this.#cachedRecycleTimes;
    }

    if (this.#loadPromise) {
      return this.#loadPromise;
    }

    this.#loadPromise = this.#readRecycleTimesFromDisk();

    try {
      this.#cachedRecycleTimes = await this.#loadPromise;
      return this.#cachedRecycleTimes;
    } finally {
      this.#loadPromise = null;
    }
  }

  async #readRecycleTimesFromDisk(): Promise<number[]> {
    try {
      const parsed = JSON.parse(await readFile(this.#filePath(), "utf8")) as RecycleFile;

      if (!Array.isArray(parsed.recycles)) {
        return [];
      }

      return parsed.recycles
        .filter((value): value is number => Number.isFinite(value))
        .sort((left, right) => left - right);
    } catch {
      return [];
    }
  }

  async #replaceRecycleTimes(recycleTimes: readonly number[]): Promise<void> {
    const mutation = this.#pendingMutation.then(async () => {
      await this.#writeRecycleTimes(this.#sortedRecycleTimes(recycleTimes));
    });

    this.#pendingMutation = mutation.catch(() => undefined);
    await mutation;
  }

  async #mutateRecycleTimes(updater: (recycleTimes: readonly number[]) => readonly number[]): Promise<void> {
    const mutation = this.#pendingMutation.then(async () => {
      const current = await this.#loadRecycleTimes();
      const next = this.#sortedRecycleTimes(updater(current));
      await this.#writeRecycleTimes(next);
    });

    this.#pendingMutation = mutation.catch(() => undefined);
    await mutation;
  }

  async #writeRecycleTimes(recycleTimes: readonly number[]): Promise<void> {
    await mkdir(this.#storagePath, { recursive: true });
    const sorted = this.#sortedRecycleTimes(recycleTimes);

    await writeFile(
      this.#filePath(),
      JSON.stringify({ recycles: sorted }),
      "utf8",
    );

    this.#cachedRecycleTimes = sorted;
  }

  #recentRecycleTimes(recycleTimes: readonly number[], now: number): number[] {
    return recycleTimes.filter((timestamp) => now - timestamp <= RECYCLE_WINDOW_MS);
  }

  #sortedRecycleTimes(recycleTimes: readonly number[]): number[] {
    return recycleTimes
      .filter((value): value is number => Number.isFinite(value))
      .sort((left, right) => left - right);
  }

  #filePath(): string {
    return path.join(this.#storagePath, RECYCLE_FILE_NAME);
  }
}
