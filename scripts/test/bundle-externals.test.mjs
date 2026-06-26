import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

/**
 * ESM-only packages that the extension uses at runtime.
 * If any of these appear as bare `require("...")` calls in the CJS bundle,
 * Node / VS Code will throw ERR_REQUIRE_ESM on activation.
 */
const ESM_ONLY_DEPS = ["p-queue"];

const bundlePath = new URL("../../extension/dist/extension.cjs", import.meta.url);

test("CJS bundle must not externalize ESM-only dependencies", () => {
  let bundle;
  try {
    bundle = readFileSync(bundlePath, "utf8");
  } catch {
    assert.fail(
      `Bundle not found at ${bundlePath.pathname}. Run "pnpm build" first.`,
    );
  }

  for (const dep of ESM_ONLY_DEPS) {
    assert.doesNotMatch(
      bundle,
      new RegExp(`require\\(["']${dep}["']\\)`),
      `Found externalized require("${dep}") in the CJS bundle. ` +
        `Add "${dep}" to alwaysBundle / onlyBundle in tsdown.config.ts.`,
    );
  }
});
