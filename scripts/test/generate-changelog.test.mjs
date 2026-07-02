import assert from "node:assert/strict";
import test from "node:test";
import {
  buildRequestBody,
  buildUserPrompt,
  COMMIT_GROUPS,
  extractContent,
  isUsableChangelog,
  renderPlainChangelog,
  resolveRange,
  SYSTEM_PROMPT,
} from "../generate-changelog.mjs";

test("resolveRange diffs from the previous tag, or full history when there is none", () => {
  assert.equal(resolveRange("v0.1.0"), "v0.1.0..HEAD");
  assert.equal(resolveRange(null), "HEAD");
});

test("buildUserPrompt lists the version and every commit subject as a bullet", () => {
  const prompt = buildUserPrompt("0.2.0", ["feat: a", "fix: b"]);
  assert.match(prompt, /Release version: 0\.2\.0/);
  assert.match(prompt, /^- feat: a$/mu);
  assert.match(prompt, /^- fix: b$/mu);
});

test("buildRequestBody produces a low-temperature OpenAI-compatible payload", () => {
  const body = buildRequestBody("some-model", "0.2.0", ["feat: a"]);
  assert.equal(body.model, "some-model");
  assert.equal(body.temperature, 0.2);
  assert.equal(body.messages[0].role, "system");
  assert.equal(body.messages[0].content, SYSTEM_PROMPT);
  assert.equal(body.messages[1].role, "user");
  assert.match(body.messages[1].content, /- feat: a/);
});

test("extractContent reads the assistant message, or null when absent", () => {
  assert.equal(extractContent({ choices: [{ message: { content: "  hello  " } }] }), "hello");
  assert.equal(extractContent({}), null);
  assert.equal(extractContent({ choices: [{ message: {} }] }), null);
  assert.equal(extractContent(null), null);
});

test("isUsableChangelog rejects empty or whitespace-only text", () => {
  assert.equal(isUsableChangelog("real notes"), true);
  assert.equal(isUsableChangelog("   \n  "), false);
  assert.equal(isUsableChangelog(""), false);
  assert.equal(isUsableChangelog(undefined), false);
});

test("renderPlainChangelog groups conventional commits and buckets the rest under Other", () => {
  const md = renderPlainChangelog([
    "feat: add inlay hints",
    "feat(cache)!: change layout",
    "fix: correct off-by-one",
    "perf: speed up parsing",
    "docs: update readme",
    "refactor: tidy module",
    "chore: bump deps",
    "random commit without prefix",
  ]);

  assert.match(md, /### Features\n- add inlay hints\n- change layout/);
  assert.match(md, /### Bug Fixes\n- correct off-by-one/);
  assert.match(md, /### Performance\n- speed up parsing/);
  assert.match(md, /### Documentation\n- update readme/);
  assert.match(md, /### Refactoring\n- tidy module/);
  assert.match(md, /### Other\n- chore: bump deps\n- random commit without prefix/);
});

test("renderPlainChangelog never returns empty text", () => {
  assert.equal(renderPlainChangelog([]), "- No notable changes.");
});

test("COMMIT_GROUPS covers the conventional prefixes we rely on", () => {
  assert.deepEqual(
    COMMIT_GROUPS.map((g) => g.prefix),
    ["feat", "fix", "perf", "docs", "refactor"],
  );
});
