import type { ChildProcessWithoutNullStreams } from "node:child_process";
import type { Readable } from "node:stream";
import type { ImportLensLogger } from "../logger.js";

export interface DisposableIpcClient {
  dispose(): void;
}

export interface KillableDaemonProcess {
  readonly exitCode: number | null;
  readonly signalCode: NodeJS.Signals | null;
  kill(signal?: NodeJS.Signals | number): boolean;
}

export interface DaemonLogStreams {
  readonly stdout: Readable;
  readonly stderr: Readable;
}

type DaemonLogLogger = Pick<ImportLensLogger, "debug" | "warn">;

export const cleanupFailedDaemonStartup = (
  client: DisposableIpcClient | null,
  childProcess: KillableDaemonProcess | null,
): void => {
  client?.dispose();

  if (!childProcess || childProcess.exitCode !== null || childProcess.signalCode !== null) {
    return;
  }

  childProcess.kill();
};

export const pipeDaemonProcessLogs = (
  childProcess: Pick<ChildProcessWithoutNullStreams, "stdout" | "stderr"> | DaemonLogStreams,
  logger: DaemonLogLogger,
): void => {
  pipeStreamLines(childProcess.stdout, "stdout", (line) => logger.debug(`[daemon:stdout] ${line}`));
  pipeStreamLines(childProcess.stderr, "stderr", (line) => logger.warn(`[daemon:stderr] ${line}`));
};

const pipeStreamLines = (
  stream: Readable,
  streamName: "stdout" | "stderr",
  writeLine: (line: string) => void,
): void => {
  let pending = "";

  stream.setEncoding("utf8");
  stream.on("data", (chunk: string | Buffer) => {
    pending += chunk.toString();
    const lines = pending.split(/\r?\n/);
    pending = lines.pop() ?? "";

    for (const line of lines) {
      writeNonEmptyLine(line, writeLine);
    }
  });
  stream.on("end", () => {
    writeNonEmptyLine(pending, writeLine);
    pending = "";
  });
  stream.on("error", (error) => {
    writeLine(`${streamName} stream error: ${error instanceof Error ? error.message : String(error)}`);
  });
};

const writeNonEmptyLine = (line: string, writeLine: (line: string) => void): void => {
  const trimmed = line.trim();

  if (trimmed.length > 0) {
    writeLine(trimmed);
  }
};
