import assert from "node:assert/strict";
import test from "node:test";
import { parseDaemonLogLine } from "../../src/logging/daemonLogParser.js";

test("parseDaemonLogLine parses structured daemon output", () => {
  assert.deepEqual(
    parseDaemonLogLine("[import-lens-daemon] 2026-06-15T12:00:00.000Z [WARN] [cache] failed to flush"),
    {
      level: "warn",
      component: "cache",
      message: "failed to flush",
    },
  );
});

test("parseDaemonLogLine parses lines without a component", () => {
  assert.deepEqual(
    parseDaemonLogLine("[import-lens-daemon] 2026-06-15T12:00:00.000Z [INFO] daemon ready"),
    {
      level: "info",
      component: undefined,
      message: "daemon ready",
    },
  );
});

test("parseDaemonLogLine returns null for unstructured lines", () => {
  assert.equal(parseDaemonLogLine("startup ready"), null);
});
