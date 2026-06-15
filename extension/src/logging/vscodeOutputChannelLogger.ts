import * as vscode from "vscode";
import type { LogLevel } from "../ipc/protocol.js";
import { defaultLogLevel, formatLogLine, shouldWriteLog } from "./loggerCore.js";
import type { LogContext, Logger } from "./types.js";

export class VscodeOutputChannelLogger implements Logger, vscode.Disposable {
  readonly #channel: vscode.OutputChannel;
  readonly #context: LogContext;
  readonly #levelRef: { value: LogLevel };
  #writeLine: (line: string) => void;
  readonly #ownsChannel: boolean;

  constructor(
    level: LogLevel = defaultLogLevel,
    context: LogContext = {},
    channel?: vscode.OutputChannel,
    writeLine?: (line: string) => void,
    ownsChannel: boolean = channel === undefined,
    levelRef?: { value: LogLevel },
  ) {
    this.#levelRef = levelRef ?? { value: level };
    this.#context = context;
    this.#channel = channel ?? vscode.window.createOutputChannel("ImportLens");
    this.#ownsChannel = ownsChannel;
    this.#writeLine = writeLine ?? ((line) => this.#channel.appendLine(line));

    if (ownsChannel) {
      this.#writeAlways("info", `Output channel initialized. Current log level: ${level}.`);
    }
  }

  setLevel(level: LogLevel): void {
    this.#levelRef.value = level;
    this.#writeAlways("info", `Log level changed to ${level}.`);
  }

  error(message: string): void {
    this.#write("error", message);
  }

  warn(message: string): void {
    this.#write("warn", message);
  }

  info(message: string): void {
    this.#write("info", message);
  }

  debug(message: string): void {
    this.#write("debug", message);
  }

  child(context: Partial<LogContext>): Logger {
    return new VscodeOutputChannelLogger(
      this.#levelRef.value,
      { ...this.#context, ...context },
      this.#channel,
      this.#writeLine,
      false,
      this.#levelRef,
    );
  }

  show(): void {
    this.#writeAlways("info", `Output channel opened. Current log level: ${this.#levelRef.value}.`);
    this.#channel.show(true);
  }

  dispose(): void {
    if (this.#ownsChannel) {
      this.#channel.dispose();
    }
  }

  #write(level: LogLevel, message: string): void {
    if (!shouldWriteLog(this.#levelRef.value, level)) {
      return;
    }

    this.#writeAlways(level, message);
  }

  #writeAlways(level: LogLevel, message: string): void {
    this.#writeLine(formatLogLine(level, message, this.#context));
  }
}
