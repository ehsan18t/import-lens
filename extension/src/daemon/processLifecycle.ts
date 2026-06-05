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

export interface WaitableDaemonProcess extends KillableDaemonProcess {
  once(event: "exit", listener: () => void): this;
  off(event: "exit", listener: () => void): this;
}

export interface DaemonLogStreams {
  readonly stdout: Readable;
  readonly stderr: Readable;
}

type DaemonLogLogger = Pick<ImportLensLogger, "info" | "warn">;

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
  pipeStreamLines(childProcess.stdout, "stdout", (line) => logger.info(`[daemon:stdout] ${line}`));
  pipeStreamLines(childProcess.stderr, "stderr", (line) => logger.warn(`[daemon:stderr] ${line}`));
};

export const terminateProcess = async (
  childProcess: ChildProcessWithoutNullStreams | WaitableDaemonProcess,
): Promise<void> => {
  if (await waitForExit(childProcess, 5000)) {
    return;
  }

  if (process.platform === "win32") {
    childProcess.kill();
    await waitForExit(childProcess, 2000);
    return;
  }

  childProcess.kill("SIGTERM");

  if (!(await waitForExit(childProcess, 2000))) {
    childProcess.kill("SIGKILL");
    await waitForExit(childProcess, 1000);
  }
};

const waitForExit = (
  childProcess: ChildProcessWithoutNullStreams | WaitableDaemonProcess,
  timeoutMs: number,
): Promise<boolean> => {
  if (childProcess.exitCode !== null || childProcess.signalCode !== null) {
    return Promise.resolve(true);
  }

  return new Promise((resolve) => {
    const onExit = (): void => {
      clearTimeout(timer);
      resolve(true);
    };
    const timer = setTimeout(() => {
      childProcess.off("exit", onExit);
      resolve(false);
    }, timeoutMs);

    childProcess.once("exit", onExit);
  });
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
