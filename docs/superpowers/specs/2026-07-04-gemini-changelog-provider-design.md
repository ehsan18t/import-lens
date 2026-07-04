# Design: Multi-provider AI changelog with a fallback chain

**Date:** 2026-07-04
**Status:** Approved (pending spec review)
**Scope:** `scripts/generate-changelog.mjs`, its unit tests, `cliff.toml`, `.github/workflows/release.yml`, `docs/release-setup-guide.md`

Two capabilities in one change:
1. **Multi-provider AI** with a Gemini-first fallback chain.
2. **Attribution** — inline commit/PR references on every changelog line plus a
   `## Contributors` section, applied uniformly to every render path.

## Problem

`scripts/generate-changelog.mjs` generates AI release notes through a single
OpenAI-compatible provider configured by `AI_API_KEY` / `AI_BASE_URL` / `AI_MODEL`
(default **Groq**, `llama-3.3-70b-versatile`). We want to add **Google Gemini**
(`gemini-3.5-flash`, free tier: 15 requests/min, 1,500 requests/day — ample for
release changelogs) as the preferred provider, and fall back to the existing
provider when it fails, without losing the deterministic safety nets.

Desired end-to-end chain:

```
Gemini  →  Groq  →  Custom (back-compat AI_*)  →  git-cliff  →  plain git-log
```

## Key insight — no new API plumbing

The script already speaks the OpenAI chat-completions protocol
(`buildRequestBody`, `extractContent`, `isUsableChangelog`, `SYSTEM_PROMPT`, the
`/chat/completions` POST with `Authorization: Bearer`). Google ships an
**OpenAI-compatible endpoint** at
`https://generativelanguage.googleapis.com/v1beta/openai/chat/completions` that
accepts exactly this request shape and Bearer auth. Gemini is therefore just
another provider **config** — only base URL, model, and key differ. The
request/response transport (`buildRequestBody`, `extractContent`,
`isUsableChangelog`, the POST/timeout/error handling) is **untouched**; the
Gemini work is purely provider selection.

(The separate attribution work below *does* touch the prompt, the plain-log
render, and `cliff.toml` — to emit inline reference tokens — but not the HTTP
transport.)

## Design

### Provider registry

A built-in registry gives each named provider its defaults. Only the key env var
is required; base URL and model have sensible defaults and are env-overridable.

| Provider | Key env var | Default base URL | Default model | Model override |
|---|---|---|---|---|
| `gemini` | `GEMINI_API_KEY` | `https://generativelanguage.googleapis.com/v1beta/openai` | `gemini-3.5-flash` | `GEMINI_MODEL` |
| `groq`   | `GROQ_API_KEY`   | `https://api.groq.com/openai/v1` | `llama-3.3-70b-versatile` | `GROQ_MODEL` |
| `custom` | `AI_API_KEY`     | `AI_BASE_URL` (default = Groq base) | `AI_MODEL` (default = Groq model) | via `AI_MODEL` |

The `custom` slot preserves two existing behaviors exactly:
- **Back-compat:** a setup with only `AI_API_KEY` set (Gemini/Groq absent)
  resolves `custom` to the Groq defaults → behaves identically to today.
- **Arbitrary endpoint:** the documented "repoint `AI_BASE_URL` to
  Cerebras/OpenRouter/…" use case still works through `custom`.

### `resolveProviders(env)` — new pure function

Returns an **ordered** array of `{ name, apiKey, baseUrl, model }`, including only
providers whose key env var is present, in fixed priority **Gemini → Groq →
Custom**. Base URLs are trailing-slash-normalized (as `callAi` does today).
Returns `[]` when no key is set (→ deterministic fallback, no AI attempted).

Fixed order (not configurable) is intentional YAGNI: it matches the stated goal
"default to Gemini, fall back to Groq." No `AI_PROVIDER_ORDER` knob.

### `callProvider(provider, version, commits)` — refactor of `callAi`

The current `callAi` body, parameterized on a provider config instead of reading
env vars directly. Unchanged: 60s timeout (now **per provider**), request body,
`extractContent`, the "empty/malformed" guard, HTTP-status error. Throws on
failure so the chain can move on.

### `generateWithAi(providers, version, commits, attempt = callProvider)` — new

Iterates the resolved providers in order:
- On success (usable changelog) → return `{ text, provider: name }`.
- On failure → `console.warn` naming the provider and reason, continue.
- All fail (or list empty) → return `null`.

`attempt` is injected (defaults to `callProvider`) purely as a **test seam** — it
lets the fallback logic be unit-tested without network access.

### `main()` wiring

```
const providers = resolveProviders(process.env);
if (providers.length > 0) {
  const result = await generateWithAi(providers, version, commits);
  if (result) { notes = result.text; console.log(`Changelog rendered by AI (${result.provider}).`); }
  else        { console.warn("All AI providers failed; falling back to git-cliff."); }
} else {
  console.log("No AI provider configured; using git-cliff.");
}
// unchanged: git-cliff → plain git-log fallbacks
```

## Attribution: inline references + Contributors section

Every changelog — regardless of which path produced it — must (a) carry inline
reference(s) to the source commit/PR on each line and (b) end with a
`## Contributors` section listing the authors. The linking and the contributors
list are computed **deterministically by us from git data**, never by the AI, so
attribution is identical and reliable across all paths.

### Data model change

`collectCommits` now captures `{ hash, short, author, subject, body }` using
`git log --pretty=%H%x1f%an%x1f%s%n%b%x00` (unit-separator between fields, NUL
between records). `short = hash.slice(0, 7)` — a consistent 7-char id used
everywhere, matching git-cliff's truncation so one linkifier handles all paths.

### Inline references per path

Each path emits **bare tokens** — `(short)` and any `#N` already in the message —
which the linkifier (below) later turns into links:

- **plain** (`renderPlainChangelog`): append `(short)` to each bullet; existing
  `#N` in the subject is preserved.
- **git-cliff** (`cliff.toml`): the body template appends
  `({{ commit.id | truncate(length=7, end="") }})` to each entry.
- **AI**: `buildUserPrompt` prefixes each commit with its short hash
  (`- [abc1234] feat: subject`); `SYSTEM_PROMPT` gains a rule: *end every bullet
  with the short hash(es) of the commit(s) it summarizes, in parentheses —
  `(abc1234)` or `(abc1234, def5678)` when merging — using only the provided
  hashes, inventing none.*

### Deterministic linkifier (git-only, no token)

- `parseRepoUrl(remote)` — normalizes `git remote get-url origin` (https **and**
  `git@` ssh forms, strips a trailing `.git`) to `https://github.com/owner/repo`,
  or `null` if unparseable.
- `linkifyRefs(body, { repoUrl, shortHashes })` — runs over the **final body of
  every path** and replaces:
  - each known short hash → `[short](repoUrl/commit/short)`
  - each `#N` → `[#N](repoUrl/issues/N)` (GitHub redirects `/issues/N` to the PR
    when `N` is a PR, so one form covers both)

  Only hashes in `shortHashes` (the actual range) are linked, so stray hex in
  prose is never touched. If `repoUrl` is `null`, tokens are left as plain text —
  still visible, just unlinked (no failure).

### Contributors section

`renderContributors(commits)` builds, from the unique `%an` authors in the range
(sorted, deduped):

```
## Contributors
- Ehsan Khan
```

Empty string when there are no commits. Appended by us to the finalized body of
**every** path — the AI is never asked to enumerate authors.

### Finalization wiring

A single `finalizeNotes(body, commits, repoUrl)` = `linkifyRefs(body, …)` +
`\n\n` + `renderContributors(commits)` is applied in `main()` to whatever body
the chain produced (AI, git-cliff, or plain), so linking + contributors are
uniform and path-agnostic.

### CI workflow (`.github/workflows/release.yml`)

Add two optional secret-backed env vars to the `release` job alongside the
existing ones (all remain optional; no preflight change — AI is never required):

```yaml
GEMINI_API_KEY: ${{ secrets.GEMINI_API_KEY }}
GROQ_API_KEY:   ${{ secrets.GROQ_API_KEY }}
AI_API_KEY:     ${{ secrets.AI_API_KEY }}   # kept for back-compat
AI_BASE_URL:    ${{ vars.AI_BASE_URL }}
AI_MODEL:       ${{ vars.AI_MODEL }}
```

(Optional: `GEMINI_MODEL` / `GROQ_MODEL` repo variables if a model override is
ever wanted; not required for the default.)

### Docs

Update `docs/release-setup-guide.md` §2.4 (and the variable table in §2.1 / the
checklist) to document `GEMINI_API_KEY` / `GROQ_API_KEY`, the provider chain, and
the Gemini free-tier note. The dated historical spec docs are left as-is.

## Testing

New unit tests in `scripts/test/generate-changelog.test.mjs` (pure, no network):

**`resolveProviders`:**
- `{}` → `[]`.
- only `GEMINI_API_KEY` → `[gemini]` with the Gemini defaults.
- `GEMINI_API_KEY` + `GROQ_API_KEY` → `[gemini, groq]` in that order.
- only `AI_API_KEY` → `[custom]` resolving to the Groq default base + model (back-compat proof).
- `GEMINI_MODEL` / `GROQ_MODEL` / `AI_BASE_URL` overrides are applied.

**`generateWithAi`** (with a fake `attempt`):
- first provider succeeds → returns it; second `attempt` never called.
- first throws, second succeeds → returns second; warn emitted for the first.
- all throw → returns `null`.
- empty provider list → returns `null` without calling `attempt`.

**Attribution (all pure, no network):**
- `parseRepoUrl` — https URL, `git@github.com:owner/repo.git` ssh, trailing
  `.git` stripping, and a junk string → `null`.
- `linkifyRefs` — a known short hash becomes a commit link; a `#N` becomes an
  issue/PR link; an unknown hex token is left untouched; `repoUrl: null` leaves
  all tokens as plain text.
- `renderContributors` — dedupes and sorts authors; `[]` → empty string.
- `renderPlainChangelog` — each bullet now ends with its `(short)` ref (test
  updated for the new signature `renderPlainChangelog(commits)`).
- `buildUserPrompt` — commit lines are prefixed with `[short]` (test updated).

Existing tests are updated where signatures changed (`renderPlainChangelog`,
`buildUserPrompt`); all others remain green.

## Non-goals / YAGNI

- No configurable provider ordering.
- No per-provider retry/backoff (a failed provider just yields to the next).
- No streaming, no non-OpenAI (native `generateContent`) Gemini path — the
  OpenAI-compat endpoint covers our need.
- No GitHub API calls for attribution — refs come only from git data and any
  `#N` the author wrote; no enrichment of PR authors into `@handles`.
- No per-bullet author attribution — authorship is summarized once in the
  `## Contributors` section, not attached to individual lines.

## Acceptance criteria

1. With `GEMINI_API_KEY` set, notes are generated by Gemini; log says `... (gemini).`
2. Gemini failing (bad key / HTTP error / empty) falls to Groq when `GROQ_API_KEY` is set, then to `custom`, then git-cliff, then plain — release never fails on changelog.
3. A pre-existing setup with only `AI_API_KEY` behaves exactly as before.
4. No AI keys set → git-cliff path, no AI call attempted, no preflight failure.
5. `resolveProviders` and `generateWithAi` are covered by network-free unit tests; the full suite passes.
6. Every render path (AI, git-cliff, plain) produces a changelog whose lines carry inline commit-hash (and `#N`, when present) links to `github.com/ehsan18t/import-lens`, and ends with a `## Contributors` section listing the range's authors.
7. An AI bullet that merges several commits shows all their short hashes inline.
8. With an unparseable/absent remote, references still render as plain text and nothing fails.
9. `parseRepoUrl`, `linkifyRefs`, `renderContributors`, and the updated `renderPlainChangelog` are covered by network-free unit tests.
