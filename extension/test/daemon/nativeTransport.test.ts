import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { PassThrough } from "node:stream";
import test from "node:test";
import type { DaemonLauncher } from "../../src/daemon/launcher.js";
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

class FakeDaemonProcess extends EventEmitter {
  readonly pid = 4242;
  readonly stdout = new PassThrough();
  readonly stderr = new PassThrough();
  exitCode: number | null = null;
  signalCode: NodeJS.Signals | null = null;
  killCount = 0;

  kill(): boolean {
    this.killCount++;
    if (this.exitCode === null && this.signalCode === null) {
      this.signalCode = "SIGTERM";
      queueMicrotask(() => this.emit("exit", null, "SIGTERM"));
    }
    return true;
  }
}

/** A launcher whose binary always verifies and whose daemon never accepts a connection. */
const refusingLauncher = (spawned: FakeDaemonProcess[]): DaemonLauncher => ({
  verifyBinary: async () => true,
  spawn: () => {
    const child = new FakeDaemonProcess();
    spawned.push(child);
    return child;
  },
  connect: async () => {
    throw new Error("connection refused");
  },
});

const withTransport = async (
  run: (
    transport: NativeDaemonTransport,
    root: string,
    spawned: FakeDaemonProcess[],
  ) => Promise<void>,
): Promise<void> => {
  const root = await mkdtemp(path.join(tmpdir(), "importlens-native-transport-"));
  const spawned: FakeDaemonProcess[] = [];
  const transport = new NativeDaemonTransport(
    fakeContext(root),
    capturingLogger([]),
    () => undefined,
    () => {
      throw new Error("config is only read after a daemon connects");
    },
    refusingLauncher(spawned),
  );

  try {
    await run(transport, root, spawned);
  } finally {
    await transport.shutdown();
    await rm(root, { recursive: true, force: true });
  }
};

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

test("concurrent start() calls share one attempt and spawn one daemon", async () => {
  await withTransport(async (transport, root, spawned) => {
    const states = await Promise.all([transport.start(root), transport.start(root)]);

    assert.deepEqual(states, ["unavailable", "unavailable"]);
    assert.equal(spawned.length, 1);
  });
});

test("shutdown() during a start's pre-spawn work stops that start from spawning", async () => {
  await withTransport(async (transport, root, spawned) => {
    const pending = transport.start(root);
    await transport.shutdown();

    assert.equal(await pending, "unavailable");
    assert.equal(spawned.length, 0);
  });
});
