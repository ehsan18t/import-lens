import type { ImportResult } from "../ipc/protocol.js";
import { LogDedupe } from "../logging/dedupe.js";
import type { Logger } from "../logging/types.js";

const sizeFields = [
  "raw_bytes",
  "minified_bytes",
  "gzip_bytes",
  "brotli_bytes",
  "zstd_bytes",
] as const;

const hasMeasuredSize = (result: ImportResult): boolean =>
  sizeFields.some((field) => result[field] > 0);

export const warningMessageForImportResult = (result: ImportResult): string | null => {
  if (!result.error || hasMeasuredSize(result)) {
    return null;
  }

  return `${result.specifier}: ${result.error}`;
};

export const debugMessageForImportResult = (result: ImportResult): string | null => {
  if (result.diagnostics.length === 0 && result.confidence_reasons.length === 0 && !result.error) {
    return null;
  }

  const lines = [`ImportLens diagnostics for ${result.specifier}`];

  if (result.error) {
    lines.push(`Error: ${result.error}`);
  }

  lines.push(`Confidence: ${result.confidence}`);

  for (const reason of result.confidence_reasons) {
    lines.push(`Reason: ${reason}`);
  }

  for (const diagnostic of result.diagnostics) {
    lines.push(`[${diagnostic.stage}] ${diagnostic.message}`);

    for (const detail of diagnostic.details) {
      lines.push(`  ${detail}`);
    }
  }

  return lines.join("\n");
};

export class ImportResultLogTracker {
  readonly #logger: Pick<Logger, "warn" | "debug">;
  readonly #requestId: number;
  readonly #dedupe = new LogDedupe();

  constructor(logger: Pick<Logger, "warn" | "debug">, requestId: number) {
    this.#logger = logger;
    this.#requestId = requestId;
  }

  logResult(result: ImportResult): void {
    const warning = warningMessageForImportResult(result);

    if (warning) {
      this.#dedupe.once(`warn:${this.#requestId}:${result.specifier}:${result.error ?? ""}`, () => {
        this.#logger.warn(warning);
      });
    }

    const debug = debugMessageForImportResult(result);

    if (debug) {
      this.#dedupe.once(`debug:${this.#requestId}:${result.specifier}`, () => {
        this.#logger.debug(debug);
      });
    }
  }

  logMissingResult(specifier: string, reason: string): void {
    this.#dedupe.once(`missing:${this.#requestId}:${specifier}:${reason}`, () => {
      this.#logger.warn(`${specifier}: ${reason}`);
    });
  }
}
