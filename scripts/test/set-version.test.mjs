import assert from "node:assert/strict";
import test from "node:test";
import { applyVersion } from "../set-version.mjs";

const manifest = (version) =>
  `${JSON.stringify({ name: "import-lens", version, publisher: "importlens" }, null, 2)}\n`;

test("applyVersion rewrites the version and reports the change", () => {
  const { changed, content } = applyVersion(manifest("0.1.0"), "0.2.0");

  assert.equal(changed, true);
  assert.equal(JSON.parse(content).version, "0.2.0");
});

test("applyVersion is a no-op when the version already matches", () => {
  const source = manifest("0.1.0");
  const { changed, content } = applyVersion(source, "0.1.0");

  assert.equal(changed, false);
  assert.equal(content, source);
});

test("applyVersion preserves key order and the trailing newline", () => {
  const { content } = applyVersion(manifest("0.1.0"), "0.2.0");

  assert.deepEqual(Object.keys(JSON.parse(content)), ["name", "version", "publisher"]);
  assert.equal(content.endsWith("\n"), true);
});
