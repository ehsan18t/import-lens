import assert from "node:assert/strict";
import test from "node:test";
import {
  buildRequestBody,
  buildUserPrompt,
  COMMIT_GROUPS,
  capBody,
  extractContent,
  generateWithAi,
  isBreakingCommit,
  isUsableChangelog,
  linkifyRefs,
  parseRepoUrl,
  renderContributors,
  renderPlainChangelog,
  resolveProviders,
  resolveRange,
  SYSTEM_PROMPT,
} from "../generate-changelog.mjs";

// v0.2.0 shipped six breaking commits and the generated notes announced none of them, because the
// only signal for four of them is the `!` in the subject and nothing looked for it.
test("isBreakingCommit recognises both the subject marker and the body footer", () => {
  assert.equal(
    isBreakingCommit({ subject: "feat(daemon)!: make rolldown the only bundler" }),
    true,
  );
  assert.equal(isBreakingCommit({ subject: "fix!: drop a flag" }), true);
  assert.equal(
    isBreakingCommit({ subject: "perf(daemon): cache enumeration", body: "BREAKING CHANGE: x" }),
    true,
  );
  assert.equal(isBreakingCommit({ subject: "feat(daemon): add a flag", body: "nothing" }), false);
  // A scope containing an exclamation mark must not be read as the breaking marker.
  assert.equal(isBreakingCommit({ subject: "fix(a!b): not breaking", body: "" }), false);
  assert.equal(isBreakingCommit(), false);
});

// The footer is conventionally last, which is exactly where head-truncation drops it: one real
// commit's sat at byte 3142 of a 3352-byte body and never reached the model.
test("capBody keeps a breaking footer that falls past the cap", () => {
  const footer = "BREAKING CHANGE: the daemon protocol changed";
  const long = `${"x".repeat(900)}\n\n${footer}`;
  const capped = capBody(long, 200);

  assert.ok(capped.length <= 200, `expected the budget to hold, got ${capped.length}`);
  assert.match(capped, /BREAKING CHANGE: the daemon protocol changed$/u);
  assert.equal(capBody("short body", 200), "short body");
  assert.equal(capBody(undefined, 200), "");
});

test("buildUserPrompt tags breaking commits so the model cannot miss them", () => {
  const prompt = buildUserPrompt("0.2.0", [
    { short: "9a09570", subject: "feat(daemon)!: make rolldown the only bundler", body: "" },
    { short: "abc1234", subject: "feat(ui): add a panel", body: "" },
  ]);

  assert.match(prompt, /^- \[9a09570\] \[BREAKING\] feat\(daemon\)!: /mu);
  assert.match(prompt, /^- \[abc1234\] feat\(ui\): add a panel$/mu);
});

test("the system prompt demands a Breaking Changes section first", () => {
  assert.match(SYSTEM_PROMPT, /Breaking Changes/u);
  assert.match(SYSTEM_PROMPT, /\[BREAKING\]/u);
  assert.match(SYSTEM_PROMPT, /FIRST, before all other sections/u);
});

test("resolveRange diffs from the previous tag, or full history when there is none", () => {
  assert.equal(resolveRange("v0.1.0"), "v0.1.0..HEAD");
  assert.equal(resolveRange(null), "HEAD");
});

test("buildUserPrompt lists the version and every commit with its short hash", () => {
  const prompt = buildUserPrompt("0.2.0", [
    { short: "abc1234", subject: "feat: a", body: "" },
    { short: "def5678", subject: "fix: b", body: "" },
  ]);
  assert.match(prompt, /Release version: 0\.2\.0/u);
  assert.match(prompt, /^- \[abc1234\] feat: a$/mu);
  assert.match(prompt, /^- \[def5678\] fix: b$/mu);
});

test("buildUserPrompt includes truncated commit bodies indented under the subject", () => {
  const prompt = buildUserPrompt("1.0.0", [
    { short: "abc1234", subject: "feat(x): add thing", body: "Adds the thing so users can do Y." },
  ]);
  assert.match(prompt, /- \[abc1234\] feat\(x\): add thing/u);
  assert.match(prompt, /^ {2}Adds the thing so users can do Y\.$/mu);

  const longBody = "z".repeat(900);
  const capped = buildUserPrompt("1.0.0", [
    { short: "def5678", subject: "fix: y", body: longBody },
  ]);
  assert.ok(capped.includes("z".repeat(600)));
  assert.ok(!capped.includes("z".repeat(601)));
});

test("buildRequestBody produces a low-temperature OpenAI-compatible payload", () => {
  const body = buildRequestBody("some-model", "0.2.0", [
    { short: "abc1234", subject: "feat: a", body: "" },
  ]);
  assert.equal(body.model, "some-model");
  assert.equal(body.temperature, 0.2);
  assert.equal(body.messages[0].role, "system");
  assert.equal(body.messages[0].content, SYSTEM_PROMPT);
  assert.equal(body.messages[1].role, "user");
  assert.match(body.messages[1].content, /- \[abc1234\] feat: a/u);
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

test("renderPlainChangelog groups conventional commits, buckets the rest, and appends refs", () => {
  const md = renderPlainChangelog([
    { short: "aaaaaaa", subject: "feat: add inlay hints" },
    { short: "bbbbbbb", subject: "feat(cache)!: change layout" },
    { short: "ccccccc", subject: "fix: correct off-by-one" },
    { short: "ddddddd", subject: "perf: speed up parsing" },
    { short: "eeeeeee", subject: "docs: update readme" },
    { short: "fffffff", subject: "refactor: tidy module" },
    { short: "1111111", subject: "chore: bump deps" },
    { short: "2222222", subject: "random commit without prefix" },
  ]);

  assert.match(md, /### Features\n- add inlay hints \(aaaaaaa\)\n- change layout \(bbbbbbb\)/u);
  assert.match(md, /### Bug Fixes\n- correct off-by-one \(ccccccc\)/u);
  assert.match(md, /### Performance\n- speed up parsing \(ddddddd\)/u);
  assert.match(md, /### Documentation\n- update readme \(eeeeeee\)/u);
  assert.match(md, /### Refactoring\n- tidy module \(fffffff\)/u);
  assert.match(
    md,
    /### Other\n- chore: bump deps \(1111111\)\n- random commit without prefix \(2222222\)/u,
  );
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

test("resolveProviders returns [] when no keys are set", () => {
  assert.deepEqual(resolveProviders({}), []);
});

test("resolveProviders yields present providers in Gemini-then-Groq order", () => {
  const providers = resolveProviders({ GEMINI_API_KEY: "g", GROQ_API_KEY: "q" });
  assert.deepEqual(
    providers.map((p) => p.name),
    ["gemini", "groq"],
  );
  assert.equal(providers[0].model, "gemini-3.5-flash");
  assert.equal(providers[0].baseUrl, "https://generativelanguage.googleapis.com/v1beta/openai");
  assert.equal(providers[1].baseUrl, "https://api.groq.com/openai/v1");
});

test("resolveProviders maps a bare AI_API_KEY to a Groq-default custom slot (back-compat)", () => {
  const providers = resolveProviders({ AI_API_KEY: "k" });
  assert.deepEqual(
    providers.map((p) => p.name),
    ["custom"],
  );
  assert.equal(providers[0].baseUrl, "https://api.groq.com/openai/v1");
  assert.equal(providers[0].model, "llama-3.3-70b-versatile");
});

test("resolveProviders honors model overrides and a custom base url", () => {
  const providers = resolveProviders({
    GEMINI_API_KEY: "g",
    GEMINI_MODEL: "gemini-x",
    AI_API_KEY: "k",
    AI_BASE_URL: "https://api.cerebras.ai/v1/",
    AI_MODEL: "llama-3.3-70b",
  });
  assert.equal(providers[0].model, "gemini-x");
  const custom = providers.find((p) => p.name === "custom");
  assert.equal(custom.baseUrl, "https://api.cerebras.ai/v1");
  assert.equal(custom.model, "llama-3.3-70b");
});

test("generateWithAi returns the first usable result and stops", async () => {
  const calls = [];
  const attempt = async (provider) => {
    calls.push(provider.name);
    return "notes";
  };
  const result = await generateWithAi([{ name: "gemini" }, { name: "groq" }], "1.0.0", [], attempt);
  assert.deepEqual(result, { text: "notes", provider: "gemini" });
  assert.deepEqual(calls, ["gemini"]);
});

test("generateWithAi falls through to the next provider on failure", async () => {
  const attempt = async (provider) => {
    if (provider.name === "gemini") throw new Error("boom");
    return "groq notes";
  };
  const result = await generateWithAi([{ name: "gemini" }, { name: "groq" }], "1.0.0", [], attempt);
  assert.deepEqual(result, { text: "groq notes", provider: "groq" });
});

test("generateWithAi returns null when every provider fails", async () => {
  const attempt = async () => {
    throw new Error("no");
  };
  assert.equal(await generateWithAi([{ name: "gemini" }], "1.0.0", [], attempt), null);
});

test("generateWithAi returns null and never attempts on an empty list", async () => {
  let called = false;
  const attempt = async () => {
    called = true;
    return "x";
  };
  assert.equal(await generateWithAi([], "1.0.0", [], attempt), null);
  assert.equal(called, false);
});

test("parseRepoUrl normalizes https and ssh GitHub remotes", () => {
  assert.equal(
    parseRepoUrl("https://github.com/ehsan18t/import-lens.git"),
    "https://github.com/ehsan18t/import-lens",
  );
  assert.equal(
    parseRepoUrl("https://github.com/ehsan18t/import-lens"),
    "https://github.com/ehsan18t/import-lens",
  );
  assert.equal(
    parseRepoUrl("git@github.com:ehsan18t/import-lens.git"),
    "https://github.com/ehsan18t/import-lens",
  );
  assert.equal(parseRepoUrl("not a url"), null);
  assert.equal(parseRepoUrl(""), null);
});

test("linkifyRefs links known short hashes and #N refs", () => {
  const body = "- add thing (abc1234)\n- fix bug (def5678) closes #42";
  const out = linkifyRefs(body, {
    repoUrl: "https://github.com/o/r",
    shortHashes: ["abc1234", "def5678"],
  });
  assert.match(out, /\[abc1234\]\(https:\/\/github\.com\/o\/r\/commit\/abc1234\)/u);
  assert.match(out, /\[def5678\]\(https:\/\/github\.com\/o\/r\/commit\/def5678\)/u);
  assert.match(out, /\[#42\]\(https:\/\/github\.com\/o\/r\/issues\/42\)/u);
});

test("linkifyRefs leaves unknown hex untouched", () => {
  const out = linkifyRefs("- see 9999999", {
    repoUrl: "https://github.com/o/r",
    shortHashes: ["abc1234"],
  });
  assert.equal(out, "- see 9999999");
});

test("linkifyRefs returns the body unchanged when repoUrl is null", () => {
  const body = "- add thing (abc1234) #42";
  assert.equal(linkifyRefs(body, { repoUrl: null, shortHashes: ["abc1234"] }), body);
});

test("renderContributors lists unique authors, sorted", () => {
  const md = renderContributors([{ author: "Bob" }, { author: "Alice" }, { author: "Bob" }]);
  assert.equal(md, "### Contributors\n\n- Alice\n- Bob");
});

test("renderContributors is empty when there are no commits", () => {
  assert.equal(renderContributors([]), "");
});
