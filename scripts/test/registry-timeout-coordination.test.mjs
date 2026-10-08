import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

// Drift check. After a 429 the daemon may pause every registry worker for up to
// `REGISTRY_MAX_BACKOFF_MS` and then fetch, up to `MAX_ATTEMPTS` times at `DEFAULT_TIMEOUT_MS` each
// with a growing retry delay between them, before it sends the next partial. The extension abandons
// a refresh that goes longer than its idle timeout without one. The two live in different languages
// and cannot share a constant. Raise any of the daemon's bounds and forget the extension, and every
// rate-limited refresh is marked failed while the daemon is still fetching it.

const repoFile = (relativePath) =>
  readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

const milliseconds = (expression) => {
  assert.match(expression, /^[\d_\s*]+$/u, `not a plain millisecond product: ${expression}`);
  return expression
    .replaceAll("_", "")
    .split("*")
    .reduce((product, factor) => product * Number(factor.trim()), 1);
};

const rustConstant = (source, name) => {
  const declaration = new RegExp(`pub const ${name}: (?:u64|usize) = ([^;]+);`, "u").exec(source);
  assert.ok(declaration, `the daemon must declare ${name}`);
  return milliseconds(declaration[1]);
};

test("the extension waits out the daemon's longest registry backoff and the fetch after it", () => {
  const constants = repoFile("daemon/src/registry/constants.rs");
  const backoff = rustConstant(constants, "REGISTRY_MAX_BACKOFF_MS");
  const attempts = rustConstant(constants, "MAX_ATTEMPTS");
  const attemptTimeout = rustConstant(constants, "DEFAULT_TIMEOUT_MS");
  const retryBaseDelay = rustConstant(constants, "REGISTRY_RETRY_BASE_DELAY_MS");
  // `transient_backoff_ms(attempt)` sleeps base * attempt before each retry.
  const retryDelays = retryBaseDelay * ((attempts * (attempts - 1)) / 2);
  const longestSilence = backoff + attempts * attemptTimeout + retryDelays;

  const clientIdle = /const REGISTRY_REFRESH_IDLE_TIMEOUT_MS = ([^;]+);/u.exec(
    repoFile("extension/src/daemon/nativeTransport.ts"),
  );
  assert.ok(clientIdle, "the extension must declare REGISTRY_REFRESH_IDLE_TIMEOUT_MS");
  assert.ok(
    milliseconds(clientIdle[1]) > longestSilence,
    `registry idle timeout ${clientIdle[1]} must exceed the daemon's longest silence (${longestSilence} ms)`,
  );
});
