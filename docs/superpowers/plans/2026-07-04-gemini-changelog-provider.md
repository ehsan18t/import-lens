# Gemini Changelog Provider + Attribution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Gemini-first multi-provider AI fallback chain to changelog generation, and attach inline commit/PR references plus a Contributors section to every changelog render path.

**Architecture:** The script already speaks OpenAI chat-completions, and Gemini ships an OpenAI-compatible endpoint, so providers differ only by base URL / model / key — resolved into an ordered chain (Gemini → Groq → custom `AI_*` → git-cliff → plain). Attribution is layered on top: every path emits bare `(shorthash)` / `#N` tokens, and one deterministic, git-only post-processor linkifies them and appends a computed `### Contributors` section — so linking and authorship never depend on the AI.

**Tech Stack:** Node.js ESM (`scripts/*.mjs`), the built-in `node:test` runner, git-cliff (TOML config), GitHub Actions YAML.

## Global Constraints

- **Commit convention (enforced by a git hook):** header `<type>(<scope>)!: <subject>` **≤ 72 chars**; a body of **≥ 20 non-whitespace chars** is **required**. Types: feat, fix, perf, docs, refactor, style, test, chore, ci, build.
- **Run the formatter after each task:** `pnpm lint:fix` (Biome). It must pass clean before committing.
- **Scripts test command:** `pnpm test:scripts` → `node --test "scripts/**/*.test.mjs"`.
- **All existing tests stay green**; tests updated only where a signature intentionally changed (`buildUserPrompt`, `buildRequestBody`, `renderPlainChangelog`).
- **No network in unit tests** — the AI chain is tested via an injected `attempt` function.
- **Provider defaults (verbatim):** Gemini → `gemini-3.5-flash` @ `https://generativelanguage.googleapis.com/v1beta/openai`; Groq → `llama-3.3-70b-versatile` @ `https://api.groq.com/openai/v1`.

---

## File Structure

- `scripts/generate-changelog.mjs` — **modify.** Adds the provider registry/resolution, chain runner, attribution helpers, and rewires `main()`. All new pure functions are exported for testing.
- `scripts/test/generate-changelog.test.mjs` — **modify.** New tests for the chain and attribution; updates to tests whose input signatures changed.
- `cliff.toml` — **modify.** Append the truncated commit id to each rendered entry.
- `.github/workflows/release.yml` — **modify.** Pass `GEMINI_API_KEY` / `GROQ_API_KEY` (optional secrets) to the job.
- `docs/release-setup-guide.md` — **modify.** Document the provider chain and the two new keys.

---

## Task 1: Multi-provider AI fallback chain

**Files:**
- Modify: `scripts/generate-changelog.mjs` (add registry + `resolveProviders`; refactor `callAi` → `callProvider`; add `generateWithAi`; rewire the AI branch of `main`)
- Test: `scripts/test/generate-changelog.test.mjs`

**Interfaces:**
- Consumes: existing `buildRequestBody`, `extractContent`, `isUsableChangelog`.
- Produces:
  - `resolveProviders(env) -> Array<{ name: string, apiKey: string, baseUrl: string, model: string }>` — ordered Gemini → Groq → custom, only entries whose key env var is set.
  - `generateWithAi(providers, version, commits, attempt = callProvider) -> Promise<{ text: string, provider: string } | null>`.
  - `callProvider(provider, version, commits) -> Promise<string>` (throws on failure; not exported).

- [ ] **Step 1: Write the failing tests**

Add to `scripts/test/generate-changelog.test.mjs` — first extend the import list at the top with `generateWithAi` and `resolveProviders`, then append:

```js
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `pnpm test:scripts`
Expected: FAIL — `resolveProviders`/`generateWithAi` are not exported (`... is not a function` / import errors).

- [ ] **Step 3: Add the registry and `resolveProviders`**

In `scripts/generate-changelog.mjs`, just below the `BODY_CAP` constant (before `resolveRange`), add:

```js
// Groq is both a named provider and the default for the back-compat custom slot.
const GROQ_BASE_URL = "https://api.groq.com/openai/v1";
const GROQ_MODEL = "llama-3.3-70b-versatile";

const stripTrailingSlashes = (url) => url.replace(/\/+$/u, "");

// Named providers, tried in this order. Only entries whose key env var is set
// are included. The custom slot (AI_API_KEY) is appended last for back-compat.
const PROVIDER_REGISTRY = [
  {
    name: "gemini",
    keyVar: "GEMINI_API_KEY",
    modelVar: "GEMINI_MODEL",
    baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai",
    model: "gemini-3.5-flash",
  },
  {
    name: "groq",
    keyVar: "GROQ_API_KEY",
    modelVar: "GROQ_MODEL",
    baseUrl: GROQ_BASE_URL,
    model: GROQ_MODEL,
  },
];

/**
 * Ordered list of usable AI providers from the environment: Gemini → Groq →
 * custom (AI_*). Only providers whose key env var is present are included. The
 * custom slot defaults to Groq, so a bare AI_API_KEY behaves exactly as before.
 */
export const resolveProviders = (env) => {
  const providers = [];
  for (const entry of PROVIDER_REGISTRY) {
    const apiKey = env[entry.keyVar];
    if (!apiKey) continue;
    providers.push({
      name: entry.name,
      apiKey,
      baseUrl: stripTrailingSlashes(entry.baseUrl),
      model: env[entry.modelVar] || entry.model,
    });
  }
  if (env.AI_API_KEY) {
    providers.push({
      name: "custom",
      apiKey: env.AI_API_KEY,
      baseUrl: stripTrailingSlashes(env.AI_BASE_URL || GROQ_BASE_URL),
      model: env.AI_MODEL || GROQ_MODEL,
    });
  }
  return providers;
};
```

- [ ] **Step 4: Refactor `callAi` into `callProvider` and add `generateWithAi`**

Replace the entire existing `callAi` function (the `const callAi = async (version, commits) => { ... };` block) with:

```js
/** One OpenAI-compatible chat-completion call for a single provider. Throws on failure. */
const callProvider = async (provider, version, commits) => {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 60_000);

  try {
    const response = await fetch(`${provider.baseUrl}/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${provider.apiKey}`,
      },
      body: JSON.stringify(buildRequestBody(provider.model, version, commits)),
      signal: controller.signal,
    });

    if (!response.ok) {
      throw new Error(`endpoint returned HTTP ${response.status}`);
    }

    const content = extractContent(await response.json());
    if (!isUsableChangelog(content)) {
      throw new Error("response was empty or malformed");
    }
    return content;
  } finally {
    clearTimeout(timeout);
  }
};

/**
 * Try each provider in order; return the first usable changelog tagged with the
 * provider name, or null if all fail. `attempt` is injectable for testing.
 */
export const generateWithAi = async (providers, version, commits, attempt = callProvider) => {
  for (const provider of providers) {
    try {
      const text = await attempt(provider, version, commits);
      if (!isUsableChangelog(text)) throw new Error("response was empty or malformed");
      return { text, provider: provider.name };
    } catch (error) {
      console.warn(`AI provider ${provider.name} failed (${error.message}); trying next.`);
    }
  }
  return null;
};
```

- [ ] **Step 5: Rewire the AI branch of `main`**

In `main()`, replace this block:

```js
  if (process.env.AI_API_KEY) {
    try {
      notes = await callAi(version, commits);
      console.log("Changelog rendered by AI.");
    } catch (error) {
      console.warn(`AI changelog failed (${error.message}); falling back to git-cliff.`);
    }
  } else {
    console.log("AI_API_KEY not set; using git-cliff.");
  }
```

with:

```js
  const providers = resolveProviders(process.env);
  if (providers.length > 0) {
    const result = await generateWithAi(providers, version, commits);
    if (result) {
      notes = result.text;
      console.log(`Changelog rendered by AI (${result.provider}).`);
    } else {
      console.warn("All AI providers failed; falling back to git-cliff.");
    }
  } else {
    console.log("No AI provider configured; using git-cliff.");
  }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `pnpm test:scripts`
Expected: PASS (all new and existing script tests green).

- [ ] **Step 7: Format and commit**

```bash
pnpm lint:fix
git add scripts/generate-changelog.mjs scripts/test/generate-changelog.test.mjs
git commit -F - <<'EOF'
feat(changelog): add Gemini-first multi-provider AI fallback chain

Resolve an ordered provider chain (Gemini -> Groq -> custom AI_*) from
named env vars and try each in turn, falling through to git-cliff. Keeps
a bare AI_API_KEY behaving exactly as before via the custom slot.
EOF
```

---

## Task 2: Attribution primitives (linking + contributors)

**Files:**
- Modify: `scripts/generate-changelog.mjs` (add three pure helpers)
- Test: `scripts/test/generate-changelog.test.mjs`

**Interfaces:**
- Produces:
  - `parseRepoUrl(remote: string) -> string | null` — normalizes an https/ssh GitHub remote to `https://github.com/owner/repo`.
  - `linkifyRefs(body: string, { repoUrl: string | null, shortHashes: string[] }) -> string` — links known short hashes and `#N` refs; no-op when `repoUrl` is null.
  - `renderContributors(commits: Array<{ author: string }>) -> string` — `### Contributors` list of unique sorted authors, or `""`.

- [ ] **Step 1: Write the failing tests**

Extend the import list with `linkifyRefs`, `parseRepoUrl`, `renderContributors`, then append:

```js
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `pnpm test:scripts`
Expected: FAIL — the three helpers are undefined.

- [ ] **Step 3: Implement the helpers**

In `scripts/generate-changelog.mjs`, add near the other pure render helpers (e.g. just after `renderPlainChangelog`):

```js
/** Normalize an https or ssh GitHub remote to https://github.com/owner/repo, or null. */
export const parseRepoUrl = (remote) => {
  if (!remote) return null;
  const match = remote.trim().match(/github\.com[:/](.+?)(?:\.git)?\/?$/u);
  return match ? `https://github.com/${match[1]}` : null;
};

/**
 * Turn bare reference tokens into Markdown links: known short hashes → commit
 * links, `#N` → issue/PR links (GitHub redirects /issues/N to the PR when
 * applicable). Only hashes in `shortHashes` are linked, so stray hex is left
 * alone. When `repoUrl` is null the body is returned unchanged.
 */
export const linkifyRefs = (body, { repoUrl, shortHashes }) => {
  if (!repoUrl) return body;
  let out = body;
  const hashes = [...new Set(shortHashes)].filter(Boolean);
  if (hashes.length > 0) {
    const pattern = new RegExp(`\\b(${hashes.join("|")})\\b`, "gu");
    out = out.replace(pattern, (hash) => `[${hash}](${repoUrl}/commit/${hash})`);
  }
  return out.replace(/#(\d+)/gu, (_match, number) => `[#${number}](${repoUrl}/issues/${number})`);
};

/** `### Contributors` list of the unique authors in the range, sorted; empty when none. */
export const renderContributors = (commits) => {
  const authors = [...new Set(commits.map((commit) => commit.author).filter(Boolean))].sort((a, b) =>
    a.localeCompare(b),
  );
  if (authors.length === 0) return "";
  return ["### Contributors", "", ...authors.map((author) => `- ${author}`)].join("\n");
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `pnpm test:scripts`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
pnpm lint:fix
git add scripts/generate-changelog.mjs scripts/test/generate-changelog.test.mjs
git commit -F - <<'EOF'
feat(changelog): add ref-linking and contributors helpers

Introduce parseRepoUrl, linkifyRefs, and renderContributors: deterministic,
git-only building blocks that turn bare commit/PR tokens into GitHub links
and summarize authorship, ready to layer onto every render path.
EOF
```

---

## Task 3: Attach attribution to every render path

**Files:**
- Modify: `scripts/generate-changelog.mjs` (commit data model; prompt; plain render; `getRepoUrl`; `finalizeNotes`; `main` wiring)
- Modify: `cliff.toml` (inline commit id)
- Test: `scripts/test/generate-changelog.test.mjs`

**Interfaces:**
- Consumes: `linkifyRefs`, `renderContributors`, `parseRepoUrl` (Task 2); `buildRequestBody`, `buildUserPrompt`, `SYSTEM_PROMPT`, `renderPlainChangelog`.
- Produces:
  - `collectCommits(range)` now returns `Array<{ hash, short, author, subject, body }>` (internal).
  - `buildUserPrompt(version, commits)` — commit lines prefixed with `[short]`.
  - `renderPlainChangelog(commits)` — now takes commit records; each bullet ends with `(short)`.
  - `finalizeNotes(body, commits, repoUrl)` and `getRepoUrl()` (internal).

- [ ] **Step 1: Update the existing tests for the new signatures**

In `scripts/test/generate-changelog.test.mjs`:

Replace the `buildUserPrompt lists ...` test body with:

```js
test("buildUserPrompt lists the version and every commit with its short hash", () => {
  const prompt = buildUserPrompt("0.2.0", [
    { short: "abc1234", subject: "feat: a", body: "" },
    { short: "def5678", subject: "fix: b", body: "" },
  ]);
  assert.match(prompt, /Release version: 0\.2\.0/u);
  assert.match(prompt, /^- \[abc1234\] feat: a$/mu);
  assert.match(prompt, /^- \[def5678\] fix: b$/mu);
});
```

Replace the `buildUserPrompt includes truncated ...` test body with:

```js
test("buildUserPrompt includes truncated commit bodies indented under the subject", () => {
  const prompt = buildUserPrompt("1.0.0", [
    { short: "abc1234", subject: "feat(x): add thing", body: "Adds the thing so users can do Y." },
  ]);
  assert.match(prompt, /- \[abc1234\] feat\(x\): add thing/u);
  assert.match(prompt, /^ {2}Adds the thing so users can do Y\.$/mu);

  const longBody = "z".repeat(900);
  const capped = buildUserPrompt("1.0.0", [{ short: "def5678", subject: "fix: y", body: longBody }]);
  assert.ok(capped.includes("z".repeat(600)));
  assert.ok(!capped.includes("z".repeat(601)));
});
```

In the `buildRequestBody ...` test, change the commit input and the last assertion:

```js
  const body = buildRequestBody("some-model", "0.2.0", [{ short: "abc1234", subject: "feat: a", body: "" }]);
```
```js
  assert.match(body.messages[1].content, /- \[abc1234\] feat: a/u);
```

Replace the `renderPlainChangelog groups ...` test body with:

```js
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
```

(The `renderPlainChangelog never returns empty text` test stays as-is — `renderPlainChangelog([])` still returns `"- No notable changes."`.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `pnpm test:scripts`
Expected: FAIL — `buildUserPrompt` has no `[short]` prefix; `renderPlainChangelog` doesn't append `(short)`.

- [ ] **Step 3: Extend the commit data model in `collectCommits`**

Replace the body of `collectCommits` with:

```js
const collectCommits = (range) => {
  const result = runCapture("git", [
    "log",
    range,
    "--no-merges",
    "--pretty=format:%H%x1f%an%x1f%s%n%b%x00",
  ]);
  if (result.status !== 0) {
    throw new Error(`git log failed: ${result.stderr?.trim() ?? "unknown error"}`);
  }
  return result.stdout
    .split("\0")
    .map((record) => record.trim())
    .filter((record) => record.length > 0)
    .map((record) => {
      const [firstLine, ...rest] = record.split("\n");
      const [hash = "", author = "", subject = ""] = firstLine.split("\x1f");
      return {
        hash,
        short: hash.slice(0, 7),
        author: author.trim(),
        subject: subject.trim(),
        body: rest.join("\n").trim(),
      };
    });
};
```

- [ ] **Step 4: Prefix the prompt with hashes and instruct the model to cite them**

In `buildUserPrompt`, change the header line and the map callback:

```js
export const buildUserPrompt = (version, commits) =>
  [
    `Release version: ${version}`,
    "",
    "Commits (short hash, subject, then body detail):",
    ...commits.map(({ short, subject, body }) => {
      const head = `- [${short}] ${subject}`;
      const trimmed = body ? body.slice(0, BODY_CAP).trim() : "";
      return trimmed ? `${head}\n  ${trimmed.replace(/\n/gu, "\n  ")}` : head;
    }),
  ].join("\n");
```

In `SYSTEM_PROMPT`, insert these three rules immediately **before** the final
`"- Use '### <Section>' headings ..."` line:

```js
  "- Each commit below is prefixed with its short hash in square brackets, e.g. '[abc1234]'.",
  "- End every bullet with the short hash(es) of the commit(s) it summarizes in parentheses, e.g. '(abc1234)' or '(abc1234, def5678)' when a bullet merges several commits; use only the provided hashes and never invent one.",
  "- Do not add a Contributors or authors section; it is appended automatically.",
```

- [ ] **Step 5: Append the ref to each plain-render bullet**

Change `renderPlainChangelog` to take commit records and append `(short)`:

```js
export const renderPlainChangelog = (commits) => {
  const stripPrefix = (subject, prefix) =>
    subject.replace(new RegExp(`^${prefix}(\\([^)]*\\))?!?:\\s*`, "iu"), "");

  const sections = [];
  const used = new Set();

  for (const { prefix, title } of COMMIT_GROUPS) {
    const matcher = new RegExp(`^${prefix}(\\([^)]*\\))?!?:`, "iu");
    const bullets = commits
      .filter((commit) => matcher.test(commit.subject))
      .map((commit) => {
        used.add(commit);
        return `- ${stripPrefix(commit.subject, prefix)} (${commit.short})`;
      });
    if (bullets.length > 0) sections.push(`### ${title}`, ...bullets, "");
  }

  const other = commits
    .filter((commit) => !used.has(commit))
    .map((commit) => `- ${commit.subject} (${commit.short})`);
  if (other.length > 0) sections.push("### Other", ...other, "");

  return sections.join("\n").trim() || "- No notable changes.";
};
```

- [ ] **Step 6: Add `getRepoUrl` and `finalizeNotes`, and wire them into `main`**

Add these near the other helpers (e.g. after `getPrevTag`):

```js
/** Parse the origin remote into a github.com base URL, or null if unavailable. */
const getRepoUrl = () => {
  const result = runCapture("git", ["remote", "get-url", "origin"]);
  if (result.status !== 0) return null;
  return parseRepoUrl(result.stdout);
};

/** Linkify inline refs in the body and append the Contributors section. */
const finalizeNotes = (body, commits, repoUrl) => {
  const linked = linkifyRefs(body, { repoUrl, shortHashes: commits.map((commit) => commit.short) });
  const contributors = renderContributors(commits);
  return contributors ? `${linked}\n\n${contributors}` : linked;
};
```

In `main()`, add the repo URL lookup near the top (after `const commits = collectCommits(range);`):

```js
  const repoUrl = getRepoUrl();
```

Then, immediately **before** the `const outPath = ...` line, insert:

```js
  notes = finalizeNotes(notes, commits, repoUrl);
```

- [ ] **Step 7: Append the commit id in `cliff.toml`**

In `cliff.toml`, change the entry line (line 12) from:

```
- {{ commit.message | split(pat="\n") | first | upper_first }}
```

to:

```
- {{ commit.message | split(pat="\n") | first | upper_first }} ({{ commit.id | truncate(length=7, end="") }})
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `pnpm test:scripts`
Expected: PASS.

- [ ] **Step 9: End-to-end smoke check (real git data, no AI)**

Run: `node scripts/generate-changelog.mjs 0.0.0-smoke /tmp/notes.md && cat /tmp/notes.md`
Expected: notes end with a `### Contributors` section listing `Ehsan Khan`, and entries carry `([abc1234](https://github.com/ehsan18t/import-lens/commit/abc1234))`-style commit links. (On Windows, use a scratch path instead of `/tmp`.)

- [ ] **Step 10: Format and commit**

```bash
pnpm lint:fix
git add scripts/generate-changelog.mjs scripts/test/generate-changelog.test.mjs cliff.toml
git commit -F - <<'EOF'
feat(changelog): attach inline refs and contributors to all paths

Carry commit short hashes (and #N refs) through every render path - AI,
git-cliff, and plain - then linkify them to GitHub and append a computed
Contributors section, uniformly and without trusting the AI for links.
EOF
```

---

## Task 4: Pass the new provider keys in CI

**Files:**
- Modify: `.github/workflows/release.yml:52-54`

**Interfaces:**
- Consumes: `resolveProviders` reads `GEMINI_API_KEY` / `GROQ_API_KEY` from the environment.

- [ ] **Step 1: Add the two optional secret-backed env vars**

In `.github/workflows/release.yml`, in the `release` job's `env:` block, change:

```yaml
      AI_API_KEY: ${{ secrets.AI_API_KEY }}
      AI_BASE_URL: ${{ vars.AI_BASE_URL }}
      AI_MODEL: ${{ vars.AI_MODEL }}
```

to:

```yaml
      GEMINI_API_KEY: ${{ secrets.GEMINI_API_KEY }}
      GROQ_API_KEY: ${{ secrets.GROQ_API_KEY }}
      AI_API_KEY: ${{ secrets.AI_API_KEY }}
      AI_BASE_URL: ${{ vars.AI_BASE_URL }}
      AI_MODEL: ${{ vars.AI_MODEL }}
```

- [ ] **Step 2: Sanity-check the YAML**

Run: `node -e "const y=require('node:fs').readFileSync('.github/workflows/release.yml','utf8'); if(!/GEMINI_API_KEY:/.test(y)||!/GROQ_API_KEY:/.test(y)) throw new Error('keys missing'); console.log('ok')"`
Expected: `ok`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -F - <<'EOF'
ci(release): pass optional Gemini and Groq API keys

Expose GEMINI_API_KEY and GROQ_API_KEY (both optional secrets) to the
release job so the changelog step can use the Gemini-first fallback chain.
EOF
```

---

## Task 5: Document the provider chain

**Files:**
- Modify: `docs/release-setup-guide.md` (§2.1 table, §2.4)

- [ ] **Step 1: Add the new keys to the §2.1 variable table**

In `docs/release-setup-guide.md`, in the table under §2.1 (the `| Name | Tab | Required for | ...` table), add these two rows immediately above the `AI_API_KEY` row:

```
| `GEMINI_API_KEY` | Secrets | **AI-written** changelogs via Gemini (optional, preferred) | (a token — see §2.4) |
| `GROQ_API_KEY` | Secrets | AI-written changelogs via Groq (optional fallback) | (a token — see §2.4) |
```

- [ ] **Step 2: Rewrite §2.4 to describe the chain**

Replace the §2.4 body (from the paragraph starting "The default provider is **Groq**" through the end of the Cerebras table and the sentence after it — i.e. lines 142–158) with:

```markdown
Changelog generation tries AI providers in order and falls back automatically:
**Gemini → Groq → any custom endpoint → git-cliff → plain git-log**. Set the key
for whichever provider(s) you want; each is optional and independent.

**Gemini (preferred, free):** `gemini-3.5-flash`, free tier 15 requests/min and
1,500/day — far more than a release needs.

1. Go to <https://aistudio.google.com/apikey> and create an API key (free).
2. Repo → Settings → Secrets and variables → Actions → **Secrets** → **New repository secret**.
   - Name: `GEMINI_API_KEY`. Value: the key. Save.

**Groq (fallback, free):** used if Gemini is unset or its call fails.

1. Go to <https://console.groq.com/> and create an API key (free).
2. Add it as the secret `GROQ_API_KEY`.

Set the model per provider with the optional **Variables** `GEMINI_MODEL` /
`GROQ_MODEL` if you ever want to override the defaults (`gemini-3.5-flash`,
`llama-3.3-70b-versatile`).

**Custom / any OpenAI-compatible endpoint (optional):** set the `AI_API_KEY`
secret plus the `AI_BASE_URL` / `AI_MODEL` variables. This slot is tried last and
defaults to Groq, so an existing `AI_API_KEY`-only setup keeps working unchanged.

| Variable | Groq (default) | Example alternative (Cerebras) |
| --- | --- | --- |
| `AI_BASE_URL` | `https://api.groq.com/openai/v1` | `https://api.cerebras.ai/v1` |
| `AI_MODEL` | `llama-3.3-70b-versatile` | `llama-3.3-70b` |
```

- [ ] **Step 3: Commit**

```bash
git add docs/release-setup-guide.md
git commit -F - <<'EOF'
docs(release): document the Gemini-first changelog provider chain

Describe the Gemini -> Groq -> custom fallback order, the two new optional
secrets, and the per-provider model overrides in the setup guide.
EOF
```

---

## Definition of Done

- `pnpm test:scripts` passes (chain + attribution tests green; updated signature tests green).
- The end-to-end smoke run (Task 3, Step 9) produces a changelog with inline commit links and a `### Contributors` section using only local git data.
- CI passes `GEMINI_API_KEY` / `GROQ_API_KEY`; an unset key is a no-op (AI remains optional, no preflight failure).
- A bare `AI_API_KEY` setup behaves exactly as before.
- Setup guide documents the chain and the two new secrets.
