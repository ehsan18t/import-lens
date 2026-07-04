import assert from "node:assert/strict";
import test from "node:test";
import {
  defaultLogLevel,
  formatContextPrefix,
  formatLogLine,
  shouldWriteLog,
} from "../src/loggerCore.js";

test("defaultLogLevel keeps the output channel useful without extra configuration", () => {
  assert.equal(defaultLogLevel, "info");
});

test("shouldWriteLog includes info messages at the default level and filters debug noise", () => {
  assert.equal(shouldWriteLog("info", "error"), true);
  assert.equal(shouldWriteLog("info", "warn"), true);
  assert.equal(shouldWriteLog("info", "info"), true);
  assert.equal(shouldWriteLog("info", "debug"), false);
});

test("formatContextPrefix includes component and request context", () => {
  assert.equal(
    formatContextPrefix({ component: "listener", requestId: 42, specifier: "react" }),
    "[listener] req=42 pkg=react ",
  );
});

test("formatLogLine includes an ISO timestamp and uppercase severity", () => {
  assert.equal(
    formatLogLine("warn", "daemon unavailable", {}, new Date("2026-06-05T10:20:30.000Z")),
    "2026-06-05T10:20:30.000Z [WARN] daemon unavailable",
  );
});
