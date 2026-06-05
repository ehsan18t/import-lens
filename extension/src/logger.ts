import * as vscode from "vscode";
import type { LogLevel } from "./ipc/protocol.js";
import { defaultLogLevel, formatLogLine, shouldWriteLog } from "./loggerCore.js";

export class ImportLensLogger implements vscode.Disposable {
  readonly #channel: vscode.OutputChannel;
  #level: LogLevel;

  constructor(level: LogLevel = defaultLogLevel) {
    this.#channel = vscode.window.createOutputChannel("ImportLens");
    this.#level = level;
    this.#writeAlways("info", `Output channel initialized. Current log level: ${level}.`);
  }

  setLevel(level: LogLevel): void {
    this.#level = level;
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

  show(): void {
    this.#writeAlways("info", `Output channel opened. Current log level: ${this.#level}.`);
    this.#channel.show(true);
  }

  dispose(): void {
    this.#channel.dispose();
  }

  #write(level: LogLevel, message: string): void {
    if (!shouldWriteLog(this.#level, level)) {
      return;
    }

    this.#writeAlways(level, message);
  }

  #writeAlways(level: LogLevel, message: string): void {
    this.#channel.appendLine(formatLogLine(level, message));
  }
}
