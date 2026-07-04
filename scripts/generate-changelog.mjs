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
  "- Use '### <Section>' headings and '- ' bullets. Output only the changelog, with no preamble or closing remarks.",
].join("\n");

// Cap each commit body so a few verbose commits cannot blow the prompt budget.
const BODY_CAP = 600;

/** Build the git range spec. Null prevTag means "all history". */
export const resolveRange = (prevTag) => (prevTag ? `${prevTag}..HEAD` : "HEAD");

/**
 * Format collected commits into the user prompt for the AI. Each commit is
 * `{ subject, body }`; the (truncated) body is indented beneath its subject.
 */
export const buildUserPrompt = (version, commits) =>
  [
    `Release version: ${version}`,
    "",
    "Commits (subject, then body detail):",
    ...commits.map(({ subject, body }) => {
      const trimmed = body ? body.slice(0, BODY_CAP).trim() : "";
      return trimmed ? `- ${subject}\n  ${trimmed.replace(/\n/gu, "\n  ")}` : `- ${subject}`;
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

/** Group raw commit subjects by conventional-commit prefix into Markdown (fallback of last resort). */
export const renderPlainChangelog = (subjects) => {
  const stripPrefix = (subject, prefix) =>
    subject.replace(new RegExp(`^${prefix}(\\([^)]*\\))?!?:\\s*`, "i"), "");

  const sections = [];
  const used = new Set();

  for (const { prefix, title } of COMMIT_GROUPS) {
    const matcher = new RegExp(`^${prefix}(\\([^)]*\\))?!?:`, "i");
    const bullets = subjects
      .filter((s) => matcher.test(s))
      .map((s) => {
        used.add(s);
        return `- ${stripPrefix(s, prefix)}`;
      });
    if (bullets.length > 0) sections.push(`### ${title}`, ...bullets, "");
  }

  const other = subjects.filter((s) => !used.has(s)).map((s) => `- ${s}`);
  if (other.length > 0) sections.push("### Other", ...other, "");

  return sections.join("\n").trim() || "- No notable changes.";
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
 * Commit `{ subject, body }` records in the range, excluding merges. Records are
 * NUL-delimited (`%x00`) so multi-line bodies survive intact.
 */
const collectCommits = (range) => {
  const result = runCapture("git", ["log", range, "--no-merges", "--pretty=format:%s%n%b%x00"]);
  if (result.status !== 0) {
    throw new Error(`git log failed: ${result.stderr?.trim() ?? "unknown error"}`);
  }
  return result.stdout
    .split("\0")
    .map((record) => record.trim())
    .filter((record) => record.length > 0)
    .map((record) => {
      const [subject, ...rest] = record.split("\n");
      return { subject: subject.trim(), body: rest.join("\n").trim() };
    });
};

const callAi = async (version, commits) => {
  const baseUrl = (process.env.AI_BASE_URL || "https://api.groq.com/openai/v1").replace(/\/+$/, "");
  const model = process.env.AI_MODEL || "llama-3.3-70b-versatile";
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 60_000);

  try {
    const response = await fetch(`${baseUrl}/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${process.env.AI_API_KEY}`,
      },
      body: JSON.stringify(buildRequestBody(model, version, commits)),
      signal: controller.signal,
    });

    if (!response.ok) {
      throw new Error(`AI endpoint returned HTTP ${response.status}`);
    }

    const content = extractContent(await response.json());
    if (!isUsableChangelog(content)) {
      throw new Error("AI response was empty or malformed");
    }
    return content;
  } finally {
    clearTimeout(timeout);
  }
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
  const subjects = commits.map((commit) => commit.subject);
  console.log(
    `Collected ${commits.length} commit(s) for v${version} (${prevTag ? `since ${prevTag}` : "full history"}).`,
  );

  let notes = null;

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

  if (!notes) {
    try {
      notes = runGitCliff(version, prevTag);
      console.log("Changelog rendered by git-cliff.");
    } catch (error) {
      console.warn(`git-cliff failed (${error.message}); falling back to plain git-log grouping.`);
      notes = renderPlainChangelog(subjects);
      console.log("Changelog rendered by plain git-log grouping.");
    }
  }

  const outPath = path.isAbsolute(outFile) ? outFile : path.join(repoRoot, outFile);
  writeFileSync(outPath, `${notes}\n`, "utf8");
  console.log(`Wrote release notes to ${outPath}`);
};

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  await main();
}
