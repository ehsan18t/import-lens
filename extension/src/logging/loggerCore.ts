import type { LogLevel } from "../ipc/protocol.js";
import type { LogContext } from "./types.js";

export const defaultLogLevel: LogLevel = "info";

const logRank: Record<LogLevel, number> = {
  error: 0,
  warn: 1,
  info: 2,
  debug: 3,
};

export const shouldWriteLog = (currentLevel: LogLevel, messageLevel: LogLevel): boolean =>
  logRank[messageLevel] <= logRank[currentLevel];

export const formatContextPrefix = (context: LogContext): string => {
  const parts: string[] = [];

  if (context.component) {
    parts.push(`[${context.component}]`);
  }

  if (context.requestId !== undefined) {
    parts.push(`req=${context.requestId}`);
  }

  if (context.documentUri) {
    parts.push(`uri=${context.documentUri}`);
  }

  if (context.specifier) {
    parts.push(`pkg=${context.specifier}`);
  }

  return parts.length > 0 ? `${parts.join(" ")} ` : "";
};

export const formatLogLine = (
  level: LogLevel,
  message: string,
  context: LogContext = {},
  date: Date = new Date(),
): string =>
  `${date.toISOString()} [${level.toUpperCase()}] ${formatContextPrefix(context)}${message}`;
