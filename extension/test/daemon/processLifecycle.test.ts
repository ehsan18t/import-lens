import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import {
  cleanupFailedDaemonStartup,
  pipeDaemonProcessLogs,
  routeDaemonLogLine,
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

test("routeDaemonLogLine routes structured daemon lines by level", () => {
  const messages: string[] = [];
  const logger = {
    error: (message: string) => messages.push(`error:${message}`),
    warn: (message: string) => messages.push(`warn:${message}`),
    info: (message: string) => messages.push(`info:${message}`),
    debug: (message: string) => messages.push(`debug:${message}`),
  };

  routeDaemonLogLine(
    "[import-lens-daemon] 2026-06-15T12:00:00.000Z [WARN] [cache] flush failed",
    logger,
    "stderr",
  );

  assert.deepEqual(messages, ["warn:[cache] flush failed"]);
});

test("routeDaemonLogLine falls back to stream defaults for unstructured lines", () => {
  const messages: string[] = [];
  const logger = {
    error: (message: string) => messages.push(`error:${message}`),
    warn: (message: string) => messages.push(`warn:${message}`),
    info: (message: string) => messages.push(`info:${message}`),
    debug: (message: string) => messages.push(`debug:${message}`),
  };

  routeDaemonLogLine("startup ready", logger, "stdout");
  routeDaemonLogLine("resolver failed", logger, "stderr");

  assert.deepEqual(messages, [
    "info:startup ready",
    "warn:resolver failed",
  ]);
});

test("pipeDaemonProcessLogs forwards daemon streams through routeDaemonLogLine", async () => {
  const stdout = new PassThrough();
  const stderr = new PassThrough();
  const messages: string[] = [];

  pipeDaemonProcessLogs(
    { stdout, stderr },
    {
      error: (message: string) => messages.push(`error:${message}`),
      warn: (message: string) => messages.push(`warn:${message}`),
      info: (message: string) => messages.push(`info:${message}`),
      debug: (message: string) => messages.push(`debug:${message}`),
    },
  );

  stdout.write("[import-lens-daemon] 2026-06-15T12:00:00.000Z [INFO] daemon ready\n");
  stderr.write("[import-lens-daemon] 2026-06-15T12:00:00.000Z [WARN] [ipc] resolver failed\n");
  await delay(0);

  assert.deepEqual(messages, [
    "info:daemon ready",
    "warn:[ipc] resolver failed",
  ]);
});
