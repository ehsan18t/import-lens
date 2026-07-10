import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";
import { NativeDaemonTransport } from "../../src/daemon/nativeTransport.js";
import type { Logger } from "../../src/logging/types.js";

const capturingLogger = (lines: string[]): Logger => {
  const record =
    (level: string) =>
    (message: string): void => {
      lines.push(`${level}: ${message}`);
    };
  const logger: Logger = {
    error: record("error"),
    warn: record("warn"),
    info: record("info"),
    debug: record("debug"),
    child: () => logger,
  };
  return logger;
};

const fakeContext = (root: string) => ({
  extensionPath: path.join(root, "extension"),
  storageUri: { fsPath: path.join(root, "storage") },
  globalStorageUri: { fsPath: path.join(root, "globalStorage") },
});

test("start() after shutdown() re-attempts startup instead of latching disposed", async () => {
  const root = path.join("C:", "tmp", "importlens-native-transport-test");
  const lines: string[] = [];
  const transport = new NativeDaemonTransport(
    fakeContext(root),
    capturingLogger(lines),
    () => undefined,
    () => {
      throw new Error("config is not read before daemon startup fails in this test");
    },
  );

  // shutdown() latches the disposed flag; a later explicit start() (as
  // DaemonManager.restart() performs) must revive the transport.
  await transport.shutdown();
  lines.length = 0;

  try {
    await transport.start(root);
  } catch {
    // Later startup stages (recycle-guard read, binary verification) fail
    // against the fake context; we assert only that startup was re-attempted.
  }

  assert.ok(
    lines.some((line) => line.includes("Starting Import Lens daemon")),
    `start() after shutdown() must re-attempt startup, not bail at the disposal latch; captured logs:\n${lines.join("\n")}`,
  );
});
