import type { LogLevel } from "../ipc/protocol.js";

export interface LogContext {
  readonly component?: string;
  readonly requestId?: number;
  readonly documentUri?: string;
  readonly specifier?: string;
}

export interface Logger {
  error(message: string): void;
  warn(message: string): void;
  info(message: string): void;
  debug(message: string): void;
  child(context: Partial<LogContext>): Logger;
}

export type { LogLevel };
