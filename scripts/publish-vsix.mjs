#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { appendFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { platformTargets } from "./targets.mjs";

// Both store CLIs default their token to the environment (VSCE_PAT / OVSX_PAT),
// so it is never passed in argv where the process list would expose it.
//
// --skip-duplicate makes a re-run converge: a target already on the store is
// reported as skipped instead of aborting the release. Without it a publish
// that dies partway through can never be retried.
const publishers = new Map([
  [
    "vsce",
    {
      patEnv: "VSCE_PAT",
      argv: (file) => ["exec", "vsce", "publish", "--packagePath", file, "--skip-duplicate"],
    },
  ],
  [
    "ovsx",
    {
      patEnv: "OVSX_PAT",
      argv: (file) => ["exec", "ovsx", "publish", file, "--skip-duplicate"],
    },
  ],
]);

export const maxAttempts = 3;

// The Marketplace rate-limits and 5xxs under a burst of platform packages, and
// the upload can lose its socket. Those deserve a retry; a 401 or a rejected
// package does not.
const retryableStatuses = new Set([408, 429, 500, 502, 503, 504]);
const retryablePatterns = [
  /socket hang up/iu,
  /ECONNRESET/u,
  /ETIMEDOUT/u,
  /ENOTFOUND/u,
  /EAI_AGAIN/u,
  /request timed out/iu,
  // Neither CLI reliably prints the numeric code: vsce forwards the raw
  // response body when it is non-empty, and a gateway may only name the status.
  /Too Many Requests/iu,
  /Internal Server Error/iu,
  /Bad Gateway/iu,
  /Service Unavailable/iu,
  /Gateway Time-?out/iu,
];

// vsce prints the error message alone, which is either the response body or
// `Failed request: (503)`; the marketplace also uses a bare `Unauthorized(401)`.
// ovsx instead says `The server responded with status 503: ...`.
export const statusCodesIn = (output) =>
  [...output.matchAll(/\((\d{3})\)|status(?:Code)?:?\s*(\d{3})/giu)].map((match) =>
    Number(match[1] ?? match[2]),
  );

export const isRetryable = (output) =>
  statusCodesIn(output).some((code) => retryableStatuses.has(code)) ||
  retryablePatterns.some((pattern) => pattern.test(output));

// Both CLIs land on "... Skipping publish." when --skip-duplicate absorbs an
// already-published version.
export const outcomeFor = (output) => (/Skipping publish/iu.test(output) ? "skipped" : "published");

export const targetForVsix = (file) => {
  const base = path.basename(file);
  return platformTargets.find((target) => base.includes(`-${target}-`)) ?? base;
};

export const backoffMs = (attempt) => Math.min(30_000, 5_000 * 2 ** (attempt - 1));

export const formatSummary = (results) =>
  [
    "| Target | Result | Attempts |",
    "| --- | --- | --- |",
    ...results.map(({ target, outcome, attempts }) => `| ${target} | ${outcome} | ${attempts} |`),
  ].join("\n");

export const publisherFor = (tool) => publishers.get(tool);

export const publisherArgv = (tool, file) => publishers.get(tool)?.argv(file);

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const runPublish = (publisher, file) => {
  const result = spawnSync("pnpm", publisher.argv(file), {
    encoding: "utf8",
    // pnpm is a .cmd shim on Windows; CI publishes from Linux.
    shell: process.platform === "win32",
  });

  const output = `${result.stdout ?? ""}${result.stderr ?? ""}${result.error?.message ?? ""}`;
  return { ok: !result.error && result.status === 0, output };
};

export const publishOne = async (publisher, file, { run = runPublish, sleep = delay } = {}) => {
  const target = targetForVsix(file);

  for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
    const { ok, output } = run(publisher, file);
    process.stdout.write(output);

    if (ok) {
      return { target, outcome: outcomeFor(output), attempts: attempt };
    }

    // The last attempt always returns here, so the loop cannot fall through.
    if (attempt === maxAttempts || !isRetryable(output)) {
      return { target, outcome: "failed", attempts: attempt, output };
    }

    const wait = backoffMs(attempt);
    console.log(
      `::warning::${target}: attempt ${attempt} hit a transient error; retrying in ${wait / 1000}s.`,
    );
    await sleep(wait);
  }
};

// Never stop at the first bad target: a store outage on one platform should not
// strand the other five, and the summary needs an outcome for every target.
export const publishAll = async (publisher, files, deps) => {
  const results = [];

  for (const file of files) {
    console.log(`::group::${path.basename(file)}`);
    results.push(await publishOne(publisher, file, deps));
    console.log("::endgroup::");
  }

  return results;
};

const main = async () => {
  const [tool, ...files] = process.argv.slice(2);
  const publisher = publishers.get(tool);

  if (!publisher) {
    console.error(`Usage: publish-vsix.mjs <${[...publishers.keys()].join("|")}> <vsix...>`);
    process.exit(1);
  }

  if (files.length === 0) {
    console.error(`No VSIX packages given to publish with ${tool}.`);
    process.exit(1);
  }

  if (!process.env[publisher.patEnv]) {
    console.error(`::error::${publisher.patEnv} is not set; cannot publish with ${tool}.`);
    process.exit(1);
  }

  const results = await publishAll(publisher, files);
  const summary = formatSummary(results);
  console.log(`\n${summary}`);

  if (process.env.GITHUB_STEP_SUMMARY) {
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, `\n### ${tool} publish\n\n${summary}\n`);
  }

  const failures = results.filter(({ outcome }) => outcome === "failed");

  for (const failure of failures) {
    const reason = failure.output.trim().split("\n").slice(-5).join(" ").slice(0, 400);
    console.log(`::error::${tool} failed to publish ${failure.target}: ${reason}`);
  }

  if (failures.length > 0) {
    console.error(
      `\n${failures.length} of ${results.length} targets failed. ` +
        `Published targets are skipped on a re-run, so this workflow is safe to retry.`,
    );
    process.exit(1);
  }

  console.log(`\nAll ${results.length} targets are published on ${tool}.`);
};

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  await main();
}
