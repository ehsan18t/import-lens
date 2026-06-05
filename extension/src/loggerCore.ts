import type { LogLevel } from "./ipc/protocol.js";

export const defaultLogLevel: LogLevel = "info";

const logRank: Record<LogLevel, number> = {
  error: 0,
  warn: 1,
  info: 2,
  debug: 3,
};

export const shouldWriteLog = (currentLevel: LogLevel, messageLevel: LogLevel): boolean =>
  logRank[messageLevel] <= logRank[currentLevel];

export const formatLogLine = (
  level: LogLevel,
  message: string,
  date: Date = new Date(),
): string => `${date.toISOString()} [${level.toUpperCase()}] ${message}`;
