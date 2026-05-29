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

  constructor(storagePath: string) {
    this.#storagePath = storagePath;
  }

  async shouldEnterDegradedMode(now = Date.now()): Promise<boolean> {
    const recent = this.#recentRecycleTimes(await this.readRecycleTimes(), now);
    return recent.length > MAX_RECYCLES_IN_WINDOW;
  }

  async resetAfterCleanSession(now = Date.now()): Promise<void> {
    const recycleTimes = await this.readRecycleTimes();
    const hasRecentRecycle = recycleTimes.some((timestamp) => now - timestamp <= CLEAN_SESSION_MS);

    if (!hasRecentRecycle) {
      await this.recordRecycleTimes([]);
    }
  }

  async readRecycleTimes(): Promise<number[]> {
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

  async recordRecycleTimes(recycleTimes: readonly number[]): Promise<void> {
    await mkdir(this.#storagePath, { recursive: true });
    await writeFile(
      this.#filePath(),
      JSON.stringify({ recycles: [...recycleTimes].sort((left, right) => left - right) }),
      "utf8",
    );
  }

  #recentRecycleTimes(recycleTimes: readonly number[], now: number): number[] {
    return recycleTimes.filter((timestamp) => now - timestamp <= RECYCLE_WINDOW_MS);
  }

  #filePath(): string {
    return path.join(this.#storagePath, RECYCLE_FILE_NAME);
  }
}
