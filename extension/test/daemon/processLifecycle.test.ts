import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import {
  cleanupFailedDaemonStartup,
  pipeDaemonProcessLogs,
  terminateProcess,
} from "../../src/daemon/processLifecycle.js";

test("cleanupFailedDaemonStartup disposes client and kills live daemon process", () => {
  let disposed = 0;
  let killed = 0;

  cleanupFailedDaemonStartup(
    {
      dispose: () => {
        disposed++;
      },
    },
    {
      exitCode: null,
      signalCode: null,
      kill: () => {
        killed++;
        return true;
      },
    },
  );

  assert.equal(disposed, 1);
  assert.equal(killed, 1);
});

test("cleanupFailedDaemonStartup does not kill an already exited process", () => {
  let killed = 0;

  cleanupFailedDaemonStartup(null, {
    exitCode: 0,
    signalCode: null,
    kill: () => {
      killed++;
      return true;
    },
  });

  assert.equal(killed, 0);
});

test("terminateProcess does not kill an already exited process", async () => {
  let killed = 0;

  await terminateProcess({
    exitCode: 0,
    signalCode: null,
    kill: () => {
      killed++;
      return true;
    },
    once: () => {
      throw new Error("already exited process should not wait for exit");
    },
    off: () => {
      throw new Error("already exited process should not clear exit listener");
    },
  });

  assert.equal(killed, 0);
});

test("pipeDaemonProcessLogs forwards daemon stderr through the extension logger", async () => {
  const stdout = new PassThrough();
  const stderr = new PassThrough();
  const messages: string[] = [];

  pipeDaemonProcessLogs(
    { stdout, stderr },
    {
      info: (message: string) => messages.push(`info:${message}`),
      warn: (message: string) => messages.push(`warn:${message}`),
    },
  );

  stdout.write("startup ready\n");
  stderr.write("resolver failed\n");
  await delay(0);

  assert.deepEqual(messages, [
    "info:[daemon:stdout] startup ready",
    "warn:[daemon:stderr] resolver failed",
  ]);
});
