# Release Setup Guide

A step-by-step guide to configuring and running ImportLens releases. It assumes **no prior knowledge** of the publishing tokens or the workflows — if you can open the GitHub repo settings, you can follow this.

This guide pairs with the design spec at
[`docs/superpowers/specs/2026-07-03-release-workflow-revamp-design.md`](superpowers/specs/2026-07-03-release-workflow-revamp-design.md),
which explains *why* things are shaped this way. This document explains *what to do*.

---

## 1. The big picture (read this first)

Releasing ImportLens is **two separate GitHub Actions workflows** you run by hand from the **Actions** tab:

1. **Build** (`.github/workflows/build.yml`) — compiles the extension for all 6 platforms
   (Windows x64/arm64, Linux x64/arm64, macOS x64/arm64) and stores each one as a downloadable
   **artifact**. It is incremental: if you re-run it for the same version, it only rebuilds the
   platforms that are still missing.
2. **Release** (`.github/workflows/release.yml`) — takes the artifacts the Build workflow produced
   and publishes them: it always drafts a **GitHub release**, and *optionally* publishes to the
   **VS Code Marketplace** and/or the **Open VSX** community store, depending on what you tick.

The normal flow is: **bump the version → run Build until all 6 are green → run Release.**

You only need to do the **one-time setup** in Section 2 **once** (or when a token expires). After
that, every release is just Section 5.

```
 bump version ──► Build workflow ──► (artifacts) ──► Release workflow ──► GitHub draft
                    (all 6 green)                          │              (+ optional stores)
                                                           └─► you review the draft & publish
```

---

## 2. One-time setup

You configure everything through **repository secrets and variables**. Secrets are hidden values
(tokens); variables are plain settings.

> **Where to add them:** GitHub repo → **Settings** → **Secrets and variables** → **Actions**.
> The page has two tabs: **Secrets** and **Variables**.

Here is everything you might set. **None of them are required just to draft a GitHub release** —
you only need a token for a store if you want to publish to that store.

| Name | Tab | Required for | Example / default |
| --- | --- | --- | --- |
| `VSCE_PAT` | Secrets | Publishing to the **VS Code Marketplace** | (a token — see §2.2) |
| `OVSX_PAT` | Secrets | Publishing to **Open VSX** | (a token — see §2.3) |
| `GEMINI_API_KEY` | Secrets | **AI-written** changelogs via Gemini (optional, preferred) | (a token — see §2.4) |
| `GROQ_API_KEY` | Secrets | AI-written changelogs via Groq (optional fallback) | (a token — see §2.4) |
| `AI_API_KEY` | Secrets | AI-written changelogs via a custom endpoint (optional) | (a token — see §2.4) |
| `AI_BASE_URL` | Variables | Custom AI endpoint (optional) | `https://api.groq.com/openai/v1` |
| `AI_MODEL` | Variables | Custom AI model (optional) | `llama-3.3-70b-versatile` |

If you skip the store tokens, releases still work — they just draft the GitHub release and skip the
stores. If you skip every AI key, changelogs are still generated, just by the deterministic tool
(git-cliff) instead of an AI.

Each of the following subsections tells you how to obtain one value.

---

### 2.1 Prerequisite: make sure the publisher IDs match

Two of the stores tie your uploads to a **publisher / namespace identity**. Both must equal the
`publisher` field already set in `package.json`:

```jsonc
// package.json
"publisher": "importlens",
```

So the VS Code Marketplace **publisher ID** must be `importlens`, and the Open VSX **namespace**
must be `importlens`. If they don't match, publishing is rejected. Keep this in mind for §2.2 and §2.3.

---

### 2.2 `VSCE_PAT` — VS Code Marketplace (Microsoft's official store)

Publishing to the official Marketplace uses a **Personal Access Token (PAT)** from **Azure DevOps**
(Microsoft's system), tied to a Marketplace **publisher**.

**Step A — create the publisher (once):**
1. Go to <https://marketplace.visualstudio.com/manage> and sign in with a Microsoft account.
2. Create a publisher. Set its **ID** to `importlens` (must match `package.json`).

**Step B — create the token:**
1. Go to <https://dev.azure.com/> and sign in with the **same** Microsoft account. If you have no
   organization yet, accept the prompt to create one (any name).
2. Click your avatar (top-right) → **Personal access tokens**.
3. Click **+ New Token** and set:
   - **Name:** anything, e.g. `importlens-vsce`.
   - **Organization:** **All accessible organizations** (this is important — a token scoped to a
     single org often fails).
   - **Expiration:** your choice (e.g. 1 year). Note the date; you'll re-do this step when it expires.
   - **Scopes:** click **Show all scopes**, find **Marketplace**, and check **Manage**.
4. Click **Create** and **copy the token now** — you cannot see it again.

**Step C — save it:**
- Repo → Settings → Secrets and variables → Actions → **Secrets** tab → **New repository secret**.
- Name: `VSCE_PAT`. Value: the token. Save.

---

### 2.3 `OVSX_PAT` — Open VSX (the community store)

Open VSX is the registry used by VSCodium, Cursor, Windsurf, Gitpod, and other non-Microsoft editors.

**Step A — create an account and sign the agreement (once):**
1. Go to <https://open-vsx.org/> and **Log in** (it uses your GitHub account via the Eclipse
   Foundation).
2. Open your **user settings** (avatar → Settings). You will be asked to **sign the Eclipse
   Publisher Agreement** — do that. Publishing is blocked until it's signed.

**Step B — create an access token:**
1. Still in user settings, open the **Access Tokens** section.
2. Create a new token, give it a description, and **copy it now**.

**Step C — create the namespace (once):**
The namespace `importlens` must exist and be owned by you before you can publish under it. From a
terminal in the repo:

```bash
# Uses the ovsx CLI (added as a dev dependency by this project).
pnpm exec ovsx create-namespace importlens -p <YOUR_OVSX_TOKEN>
```

(If it says the namespace already exists and is yours, you're fine.)

**Step D — save the token:**
- Repo → Settings → Secrets and variables → Actions → **Secrets** tab → **New repository secret**.
- Name: `OVSX_PAT`. Value: the token. Save.

---

### 2.4 AI-written changelogs (optional)

If a provider key is set, the Release workflow asks a free AI model to turn your commit messages into
a clean, categorized changelog for the GitHub release. If **none** is set, it uses git-cliff (a deterministic
tool) instead — so this is purely a "nicer notes" upgrade, never required.

Changelog generation tries AI providers in order and falls back automatically:
**Gemini → Groq → any custom endpoint → git-cliff → plain git-log**. Set the key for whichever
provider(s) you want; each is optional and independent.

**Gemini (preferred, free):** `gemini-3.5-flash`, free tier 15 requests/min and 1,500/day — far more
than a release needs.

1. Go to <https://aistudio.google.com/apikey> and create an API key (free).
2. Repo → Settings → Secrets and variables → Actions → **Secrets** → **New repository secret**.
   - Name: `GEMINI_API_KEY`. Value: the key. Save.

**Groq (fallback, free):** used if Gemini is unset or its call fails.

1. Go to <https://console.groq.com/> and create an API key (free).
2. Add it as the secret `GROQ_API_KEY`.

Set the model per provider with the optional **Variables** `GEMINI_MODEL` / `GROQ_MODEL` if you ever
want to override the defaults (`gemini-3.5-flash`, `llama-3.3-70b-versatile`).

**Custom / any OpenAI-compatible endpoint (optional):** set the `AI_API_KEY` secret plus the
`AI_BASE_URL` / `AI_MODEL` variables. This slot is tried last and defaults to Groq, so an existing
`AI_API_KEY`-only setup keeps working unchanged.

| Variable | Groq (default) | Example alternative (Cerebras) |
| --- | --- | --- |
| `AI_BASE_URL` | `https://api.groq.com/openai/v1` | `https://api.cerebras.ai/v1` |
| `AI_MODEL` | `llama-3.3-70b-versatile` | `llama-3.3-70b` |

> **Note:** commit messages are sent to whichever AI provider you enable. This repo's commits are
> public anyway. Groq states it does not train on your inputs; Google's **free** Gemini tier may use
> prompts to improve its products (the paid tier does not). If you ever put secrets in commit messages
> (you shouldn't), leave every AI key unset to keep everything local.

---

### 2.5 Repository permissions (usually nothing to do)

The workflows request the exact permissions they need in their own YAML (`contents: write` to create
the release, `actions: read` to fetch build artifacts). On a normal repo this just works.

If your organization restricts Actions, make sure:
- **Settings → Actions → General → Workflow permissions** allows workflows to request write access
  (the default "Read and write" or "Read repository contents and packages permissions" with
  per-job escalation is fine).
- Actions are **enabled** for the repo.

---

## 3. Setup checklist

Tick these off once. You only need the rows for the stores you actually want to publish to.

- [ ] `package.json` `publisher` is `importlens`.
- [ ] **VS Code Marketplace:** publisher `importlens` created; `VSCE_PAT` secret added.
- [ ] **Open VSX:** agreement signed; namespace `importlens` created; `OVSX_PAT` secret added.
- [ ] **AI changelog (optional):** `GEMINI_API_KEY` and/or `GROQ_API_KEY` secret added (or the custom
      `AI_API_KEY` + `AI_BASE_URL` / `AI_MODEL` variables).
- [ ] Actions are enabled and can request write permissions.

---

## 4. Before every release

1. **Bump the version** in `package.json` (e.g. `0.1.0` → `0.2.0`) and commit it to `main`. This is
   the normal path: leave the workflow `version` field **blank** and it uses `package.json`. If you
   instead type a `version`, that value **overrides** `package.json` for that run. Whatever value you
   use, use the **same one** for Build and Release.
2. Make sure `main` is in the state you want to ship (the release is built from the latest commit).

---

## 5. Cutting a release (the routine)

### Step 1 — Run the Build workflow

1. Go to the repo's **Actions** tab.
2. In the left sidebar, click **Build**.
3. Click **Run workflow** (top-right of the runs list). Fill in:
   - **version:** leave **blank** to use `package.json` (normal), or type e.g. `0.2.0` to override
     it (no leading `v`).
   - **force:** leave **unchecked** normally. Check it only if you want to rebuild every platform
     from scratch, ignoring anything already built for this version.
4. Click the green **Run workflow** button and wait. You'll see the 6 platform jobs run.

**If one platform fails:** just **run Build again** with the **same version**. The platforms that
already succeeded are restored instantly from cache (seconds), and only the failed one is rebuilt.
Repeat until all 6 are green in a single run.

> Why this saves money: native compiles (especially macOS) are the expensive part. Re-running only
> the failures avoids paying for all 6 every time.

### Step 2 — Dry-run the Release workflow (recommended)

1. Actions tab → **Release** → **Run workflow**. Fill in:
   - **version:** the **same** value you built with (blank if you built from `package.json`).
   - **release_github:** checked (on by default).
   - **publish_vscode / publish_openvsx:** tick the stores you want (off by default).
   - **dry_run:** **checked.**
2. Run it. A dry run **creates and publishes nothing.** It verifies that:
   - all 6 artifacts exist for this version,
   - every store you ticked has its token configured (it **fails fast** if, say, you ticked
     Open VSX but never added `OVSX_PAT`),
   - and it **prints a preview of the changelog** it would use.
3. Read the output. If it's happy and the changelog looks right, proceed.

### Step 3 — Run the real Release

1. Actions tab → **Release** → **Run workflow**. Same inputs as the dry run, but **dry_run
   unchecked.**
2. Pick your destinations:
   - **Just GitHub:** leave both store boxes unchecked.
   - **GitHub + Open VSX:** check `publish_openvsx`.
   - **Everywhere:** check both store boxes.
3. Run it. It will:
   - draft the GitHub release with the generated changelog and all 6 VSIX files attached,
   - publish to each store you selected.

### Step 4 — Review and publish the GitHub release

The GitHub release is created as a **draft** on purpose — nothing is public until you say so.

1. Go to the repo's **Releases** page. Open the new draft.
2. Read the auto-generated changelog. **Edit it** if the AI phrased something oddly or missed
   context (this is your safety net).
3. When happy, click **Publish release**. This makes it public and creates the `v0.2.0` git tag.

Done. 🎉

---

## 6. Choosing where to publish — quick reference

| I want to release to… | `release_github` | `publish_vscode` | `publish_openvsx` |
| --- | --- | --- | --- |
| GitHub only | ✅ | ⬜ | ⬜ |
| GitHub + VS Code Marketplace | ✅ | ✅ | ⬜ |
| GitHub + Open VSX | ✅ | ⬜ | ✅ |
| Everywhere | ✅ | ✅ | ✅ |

If you tick a store whose token isn't configured, the Release workflow **stops immediately in its
preflight check** with a clear message — before doing any work — so you never get a half-done release.

---

## 7. Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| Release fails instantly with "…secret is not configured" | You ticked a store but its token secret is missing | Add `VSCE_PAT` / `OVSX_PAT` (§2.2 / §2.3), or untick that store |
| Release fails with "Missing VSIX artifacts" | Not all 6 platforms were built for this version, or the artifacts expired (they last 1 day) | Re-run the **Build** workflow for this version, then Release again |
| Release can't find the build | The version Release resolved has no matching Build artifacts | Use the same version for Build and Release (leave both blank to use `package.json`) |
| VS Code Marketplace publish rejected | Publisher ID mismatch, or `VSCE_PAT` lacks **Marketplace → Manage** scope, or token expired | Recreate the PAT per §2.2 (All accessible organizations + Manage scope) |
| Open VSX publish rejected | Namespace `importlens` not created, or agreement unsigned | Do §2.3 Step A and Step C |
| Changelog is plain / not AI-written | No AI key set, or every configured provider failed and it fell back | Expected behavior — add/fix `GEMINI_API_KEY` (or `GROQ_API_KEY`) for AI notes; git-cliff notes are always fine to ship |
| A build platform keeps failing | A genuine compile error for that target | Open the failed job's logs; fix the code; re-run Build (only that platform rebuilds) |
| I need to rebuild everything cleanly | Stale/cached build for a version | Run Build with **force** checked |

---

## 8. Token expiry — set a reminder

`VSCE_PAT` (and possibly others) **expire**. When a publish suddenly fails with an auth error,
regenerate the relevant token using the matching subsection in Section 2 and update the secret.
Consider noting expiry dates somewhere you'll see them.
