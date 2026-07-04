import type * as vscode from "vscode";
import type { LogLevel } from "./ipc/protocol.js";
import { defaultLogLevel, VscodeOutputChannelLogger } from "./logging/index.js";
import type { Logger } from "./logging/types.js";

export type { LogContext, Logger, LogLevel } from "./logging/index.js";
export { defaultLogLevel, formatLogLine, shouldWriteLog } from "./logging/index.js";

export class ImportLensLogger extends VscodeOutputChannelLogger implements vscode.Disposable {}

export const createImportLensLogger = (level: LogLevel = defaultLogLevel): ImportLensLogger =>
  new ImportLensLogger(level);
