import type { ImportResult } from "../ipc/protocol.js";

type ResultLogSink = {
  warn(message: string): void;
  debug(message: string): void;
};

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

const debugMessageForImportResult = (result: ImportResult): string | null => {
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
  readonly #logger: ResultLogSink;
  readonly #warned = new Set<string>();
  readonly #debugged = new Set<string>();

  constructor(logger: ResultLogSink) {
    this.#logger = logger;
  }

  logResult(result: ImportResult): void {
    const warning = warningMessageForImportResult(result);

    if (warning) {
      this.#warnOnce(`result:${warning}`, warning);
    }

    const debug = debugMessageForImportResult(result);

    if (debug) {
      this.#debugOnce(`result:${result.specifier}:${debug}`, debug);
    }
  }

  logMissingResult(specifier: string, reason: string): void {
    this.#warnOnce(`missing:${specifier}:${reason}`, `${specifier}: ${reason}`);
  }

  #warnOnce(key: string, message: string): void {
    if (this.#warned.has(key)) {
      return;
    }

    this.#warned.add(key);
    this.#logger.warn(message);
  }

  #debugOnce(key: string, message: string): void {
    if (this.#debugged.has(key)) {
      return;
    }

    this.#debugged.add(key);
    this.#logger.debug(message);
  }
}
