#!/usr/bin/env node

// Generates release notes for a version by collecting commits since the previous
// `v*` tag, then rendering them either with an AI model (when AI_API_KEY is set)
// or deterministically with git-cliff. A plain grouped git-log render is the final
// safety net so the notes are never empty.
//
// Usage: node scripts/generate-changelog.mjs <version> [outFile]
//   version  e.g. 0.2.0 (no leading "v")
//   outFile  path to write notes to (default: notes.md)

import { spawnSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

// Conventional-commit prefixes → section titles, in display order. Shared by the
// AI prompt (as guidance) and the plain-log fallback (as the grouping rule).
export const COMMIT_GROUPS = [
  { prefix: "feat", title: "Features" },
  { prefix: "fix", title: "Bug Fixes" },
  { prefix: "perf", title: "Performance" },
  { prefix: "docs", title: "Documentation" },
  { prefix: "refactor", title: "Refactoring" },
];

export const SYSTEM_PROMPT = [
  "You write concise, accurate release notes for a software project.",
  "You are given a list of git commits for a single release: each commit's subject line, followed by its body detail.",
  "Produce a categorized Markdown changelog using only the information in those commits.",
  "Rules:",
  "- Never invent changes that are not present in the commits.",
  "- Use the body detail to write clearer, more user-facing bullets, but never introduce changes absent from the commits.",
  "- Group entries under these sections when applicable: Features, Bug Fixes, Performance, Documentation, Other.",
  "- Omit any section that has no entries.",
  "- One short, user-facing bullet per meaningful change; merge duplicates; drop pure noise (formatting, version bumps, merge commits).",
  "- Each commit below is prefixed with its short hash in square brackets, e.g. '[abc1234]'.",
  "- End every bullet with the short hash(es) of the commit(s) it summarizes in parentheses, e.g. '(abc1234)' or '(abc1234, def5678)' when a bullet merges several commits; use only the provided hashes and never invent one.",
  "- Do not add a Contributors or authors section; it is appended automatically.",
  "- Use '### <Section>' headings and '- ' bullets. Output only the changelog, with no preamble or closing remarks.",
].join("\n");

// Cap each commit body so a few verbose commits cannot blow the prompt budget.
const BODY_CAP = 600;

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

/** Build the git range spec. Null prevTag means "all history". */
export const resolveRange = (prevTag) => (prevTag ? `${prevTag}..HEAD` : "HEAD");

/**
 * Format collected commits into the user prompt for the AI. Each commit is
 * `{ short, subject, body }`; the short hash prefixes the subject so the model
 * can cite it, and the (truncated) body is indented beneath.
 */
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

/** Build the OpenAI-compatible chat completion request body. */
export const buildRequestBody = (model, version, commits) => ({
  model,
  temperature: 0.2,
  messages: [
    { role: "system", content: SYSTEM_PROMPT },
    { role: "user", content: buildUserPrompt(version, commits) },
  ],
});

/** Extract the assistant message text from an OpenAI-compatible response. */
export const extractContent = (json) => {
  const content = json?.choices?.[0]?.message?.content;
  return typeof content === "string" ? content.trim() : null;
};

/** A changelog is usable if it has real, non-whitespace content. */
export const isUsableChangelog = (text) => typeof text === "string" && text.trim().length > 0;

/** Group commit records by conventional-commit prefix into Markdown, each bullet
 * ending with its short-hash ref (fallback of last resort). */
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
  const authors = [...new Set(commits.map((commit) => commit.author).filter(Boolean))].sort(
    (a, b) => a.localeCompare(b),
  );
  if (authors.length === 0) return "";
  return ["### Contributors", "", ...authors.map((author) => `- ${author}`)].join("\n");
};

const runCapture = (command, args) => spawnSync(command, args, { cwd: repoRoot, encoding: "utf8" });

/** Nearest reachable `v*` tag before HEAD, or null on the first-ever release. */
const getPrevTag = () => {
  const result = runCapture("git", ["describe", "--tags", "--abbrev=0", "--match", "v*", "HEAD"]);
  if (result.status !== 0) return null;
  const tag = result.stdout.trim();
  return tag.length > 0 ? tag : null;
};

/**
 * Commit `{ hash, short, author, subject, body }` records in the range, excluding
 * merges. Fields on the first line are unit-separated (`%x1f`); records are
 * NUL-delimited (`%x00`) so multi-line bodies survive intact.
 */
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

/** Deterministic render via git-cliff over the range. Throws if git-cliff is unavailable/fails. */
const runGitCliff = (version, prevTag) => {
  const args = ["--tag", `v${version}`, "--strip", "header"];
  if (prevTag) args.push(`${prevTag}..HEAD`);
  const result = runCapture("git-cliff", args);
  if (result.status !== 0) {
    throw new Error(`git-cliff failed: ${result.stderr?.trim() ?? "not installed?"}`);
  }
  const content = result.stdout.trim();
  if (!isUsableChangelog(content)) {
    throw new Error("git-cliff produced empty output");
  }
  return content;
};

const main = async () => {
  const [version, outFile = "notes.md"] = process.argv.slice(2);
  if (!version) {
    console.error("Usage: node scripts/generate-changelog.mjs <version> [outFile]");
    process.exit(1);
  }

  const prevTag = getPrevTag();
  const range = resolveRange(prevTag);
  const commits = collectCommits(range);
  const repoUrl = getRepoUrl();
  console.log(
    `Collected ${commits.length} commit(s) for v${version} (${prevTag ? `since ${prevTag}` : "full history"}).`,
  );

  let notes = null;

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

  if (!notes) {
    try {
      notes = runGitCliff(version, prevTag);
      console.log("Changelog rendered by git-cliff.");
    } catch (error) {
      console.warn(`git-cliff failed (${error.message}); falling back to plain git-log grouping.`);
      notes = renderPlainChangelog(commits);
      console.log("Changelog rendered by plain git-log grouping.");
    }
  }

  notes = finalizeNotes(notes, commits, repoUrl);

  const outPath = path.isAbsolute(outFile) ? outFile : path.join(repoRoot, outFile);
  writeFileSync(outPath, `${notes}\n`, "utf8");
  console.log(`Wrote release notes to ${outPath}`);
};

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  await main();
}
