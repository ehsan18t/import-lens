import * as vscode from "vscode";
import type { LogLevel } from "./ipc/protocol.js";

const logRank: Record<LogLevel, number> = {
  error: 0,
  warn: 1,
  info: 2,
  debug: 3,
};

export class ImportLensLogger implements vscode.Disposable {
  readonly #channel: vscode.OutputChannel;
  #level: LogLevel;

  constructor(level: LogLevel) {
    this.#channel = vscode.window.createOutputChannel("ImportLens");
    this.#level = level;
  }

  setLevel(level: LogLevel): void {
    this.#level = level;
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
    this.#channel.show();
  }

  dispose(): void {
    this.#channel.dispose();
  }

  #write(level: LogLevel, message: string): void {
    if (logRank[level] > logRank[this.#level]) {
      return;
    }

    this.#channel.appendLine(`${new Date().toISOString()} [${level.toUpperCase()}] ${message}`);
  }
}

