import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const BUNDLED_RUNTIME_DEPS = ["@msgpack/msgpack"];

const bundlePath = new URL("../../extension/dist/extension.cjs", import.meta.url);

test("CJS bundle must not externalize bundled runtime dependencies", () => {
  let bundle;
  try {
    bundle = readFileSync(bundlePath, "utf8");
  } catch {
    assert.fail(
      `Bundle not found at ${bundlePath.pathname}. Run "pnpm build" first.`,
    );
  }

  for (const dep of BUNDLED_RUNTIME_DEPS) {
    assert.doesNotMatch(
      bundle,
      new RegExp(`require\\(["']${escapeRegExp(dep)}["']\\)`),
      `Found externalized require("${dep}") in the CJS bundle. ` +
        `Add "${dep}" to alwaysBundle / onlyBundle in tsdown.config.ts.`,
    );
  }
});

const escapeRegExp = (value) => value.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
