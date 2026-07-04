import type { LogLevel } from "../ipc/protocol.js";

export interface ParsedDaemonLogLine {
  readonly level: LogLevel;
  readonly component?: string;
  readonly message: string;
}

const structuredDaemonLogPattern =
  /^\[import-lens-daemon\]\s+(\S+)\s+\[(ERROR|WARN|INFO|DEBUG)\](?:\s+\[([^\]]+)\])?\s+(.+)$/iu;

const levelFromToken = (token: string): LogLevel | null => {
  const normalized = token.toLowerCase();

  if (
    normalized === "error" ||
    normalized === "warn" ||
    normalized === "info" ||
    normalized === "debug"
  ) {
    return normalized;
  }

  return null;
};

export const parseDaemonLogLine = (line: string): ParsedDaemonLogLine | null => {
  const match = structuredDaemonLogPattern.exec(line.trim());

  if (!match) {
    return null;
  }

  const level = levelFromToken(match[2] ?? "");

  if (!level) {
    return null;
  }

  return {
    level,
    component: match[3] || undefined,
    message: match[4] ?? "",
  };
};
