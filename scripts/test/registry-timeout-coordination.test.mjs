import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

// Drift check. The daemon may pause every registry worker for up to `REGISTRY_MAX_BACKOFF_MS`
// after a 429 and only then send the next partial; the extension abandons a refresh that goes
// longer than its idle timeout without one. The two live in different languages and cannot share a
// constant. Raise the daemon's bound and forget the extension, and every rate-limited refresh is
// marked failed while the daemon is still fetching it.

const repoFile = (relativePath) =>
  readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

const milliseconds = (expression) => {
  assert.match(expression, /^[\d_\s*]+$/u, `not a plain millisecond product: ${expression}`);
  return expression
    .replaceAll("_", "")
    .split("*")
    .reduce((product, factor) => product * Number(factor.trim()), 1);
};

test("the extension waits out the daemon's longest registry backoff", () => {
  const daemonBackoff = /pub const REGISTRY_MAX_BACKOFF_MS: u64 = ([^;]+);/u.exec(
    repoFile("daemon/src/registry/constants.rs"),
  );
  const clientIdle = /const REGISTRY_REFRESH_IDLE_TIMEOUT_MS = ([^;]+);/u.exec(
    repoFile("extension/src/daemon/nativeTransport.ts"),
  );

  assert.ok(daemonBackoff, "the daemon must declare REGISTRY_MAX_BACKOFF_MS");
  assert.ok(clientIdle, "the extension must declare REGISTRY_REFRESH_IDLE_TIMEOUT_MS");
  assert.ok(
    milliseconds(clientIdle[1]) > milliseconds(daemonBackoff[1]),
    `registry idle timeout ${clientIdle[1]} must exceed the daemon's backoff ${daemonBackoff[1]}`,
  );
});
