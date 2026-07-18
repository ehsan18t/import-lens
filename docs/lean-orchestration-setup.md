# Lean Orchestration — Setup and Usage

Lean Orchestration is a Claude Code skill that routes any non-trivial task (feature, bug hunt, review/audit, design critique, refactor) for the most output quality per token spent. It decides per phase whether to work inline or dispatch subagents, verifies findings adversarially only where that pays, and stops loops on written conditions instead of "keep going until sure."

This document covers what the skill consists of, how to set it up on a fresh machine, and how to use it day to day.

## Components

The skill is two kinds of files that work together:

| Component | Repo path | What it does |
|---|---|---|
| Skill | `.claude/skills/lean-orchestration/SKILL.md` | The routing playbook: escalation ladder, cost levers, Steps 0–11, loop stop conditions. |
| `navigator` agent | `.claude/agents/navigator.md` | Read-only role agent for "read-to-understand" questions. Returns a distilled, `path:line`-anchored answer instead of raw file dumps. |
| `navigator-lite` agent | `.claude/agents/navigator-lite.md` | Low-reasoning-effort navigator variant for purely mechanical lookups (where is X defined, what value does Y hold). Same contract, cheaper dispatch. |
| `finder` agent | `.claude/agents/finder.md` | Read-only role agent for review passes. Runs one review lens (code-defect / spec-conformance / design-critique) over one slice and returns deduped, anchored findings. |
| `skeptic` agent | `.claude/agents/skeptic.md` | Adversarial verifier. Receives findings it did not author and tries to refute them; returns `REFUTED / SURVIVES / PROVEN` verdicts with evidence. Also runs behavior-preservation checks on refactor diffs. |
| `skeptic-max` agent | `.claude/agents/skeptic-max.md` | Maximum-reasoning-effort skeptic, reserved for the proof-burden pass on critical findings (data loss, security, wedge, risky fixes) where verdicts must rest on positive proof. |

The role agents are deliberately lean: a minimal tool whitelist and all MCP servers stripped (`disallowedTools: mcp__*`), so each dispatch starts with the smallest possible fixed context instead of the full tool surface a `general-purpose` agent carries.

## Setup

### In this repository

Nothing to do. All six files above are checked in under `.claude/`, and Claude Code picks up project-level skills and agents automatically. Cloning the repo is the whole install.

### On a fresh machine, for use across all projects (optional)

If you want the skill available outside this repo, copy the files to your user-level Claude directory:

```powershell
Copy-Item -Recurse .claude\skills\lean-orchestration "$env:USERPROFILE\.claude\skills\lean-orchestration"
New-Item -ItemType Directory -Force "$env:USERPROFILE\.claude\agents" | Out-Null
Copy-Item .claude\agents\navigator.md, .claude\agents\navigator-lite.md, .claude\agents\finder.md, .claude\agents\skeptic.md, .claude\agents\skeptic-max.md "$env:USERPROFILE\.claude\agents\"
```

On macOS/Linux the destination is `~/.claude/skills/` and `~/.claude/agents/`.

When both the project and user-level copies exist, the skill and agents appear twice in Claude's listings. That is harmless — both copies are identical; keep them in sync when editing.

### Optional companion skills

Step 2.5 of the skill (clarify-before-building) escalates automatically when the companion skills are installed: `grilling` interrogates a plan's assumptions, and for a feature or consequential design Claude launches the full grill-with-docs interview itself by invoking its components (`grilling` + `domain-modeling`), capturing ADRs and a glossary as it goes; you answer the questions. `/grill-with-docs` itself stays a manual command (it is user-launch-only by design and is never modified). The skill works without them — the first two clarification rungs (state assumptions, ask targeted questions) need no extra install — but the deeper rungs fire only when those skills exist.

### Verifying the install

Ask Claude to list its available agents or skills, or simply start a non-trivial task. You should see:

- `lean-orchestration` among the available skills;
- `navigator`, `navigator-lite`, `finder`, `skeptic`, and `skeptic-max` among the available agent types;
- on a routed task, a one-line route announcement (see below) before any subagent is dispatched.

## Usage

### Invoking it

- **Explicitly:** type `/lean-orchestration <your task>`.
- **Automatically:** Claude invokes it on its own when a task matches the trigger — any non-trivial feature, bug hunt, review, audit, critique, or refactor. Single-file changes, quick lookups, and tight debug loops deliberately bypass it (orchestration overhead would exceed its value there).

### What you will see

1. **A route line**, e.g. `Route: review of branch diff (code + design), deliverable=report → ground, 2 lenses, verify contested only, no implement.` This is your veto point: if the classification is wrong (wrong deliverable, wrong scope), say so before any cost is spent. The route can also be corrected mid-task if evidence contradicts it.
2. **Possibly 1–2 clarifying questions** on features or ambiguous fixes — only when a material unknown would cause rework if guessed wrong. Crisp requests skip this.
3. **Dispatches to the role agents** as the task shape demands: navigators to understand code, finders to review it, skeptics to adjudicate contested findings.
4. **A deliverable**: an inline answer, a findings report written to a file, or implemented fixes/features verified against gates (tests, typecheck, lint) first and subagent review only for what gates cannot see.

Findings in reports are tagged with their verification level (`gate-caught` / `refuted-survived` / `proof-confirmed` / `finder-claim` / `refuted`; `finder-claim` = reported, never independently verified) so you know where to spend your own review attention.

### How it keeps cost down

Three levers, applied in this order:

1. **Escalation ladder** — deterministic gates first (free), one skeptic only for contested claims, panels only for high-blast-radius auto-applied changes. It never pays an LLM to find what a test finds for free.
2. **Effort tiering, never model tiering** — fully-specified mechanical lookups go to `navigator-lite` (same model, low reasoning effort); the model itself is never downgraded, because a smaller model spends more tokens for worse output on the same task. Review and verification always run at full effort.
3. **Lean role agents** — every dispatch uses the smallest agent definition that can do the job, so the per-subagent fixed context (tool schemas, MCP listings) stays minimal.

On top of that: shared context (specs, design docs, `known-issues.md`) is loaded and distilled once and injected into workers rather than re-read N times; duplicate findings are collapsed to root cause before verification; small related findings share one verifier; and every loop has a written stop condition.

### When NOT to expect it

By design, the skill stays out of the way for small work. If you ask for a one-file tweak or a quick factual question and Claude just does it inline with no route line — that is the skill's Step 0 working, not the skill failing to trigger.

## Maintenance notes

- The project copy under `.claude/` is the source of truth. If you also installed user-level copies, mirror any edits to both.
- The role agents' return formats (`FINDING/ANCHORS/…`, `FINDINGS/NOTES/COVERAGE`, `FINDING/DEFAULT/VERDICT/EVIDENCE/REASONING`) are contracts the skill's steps consume — change them in the agent file and the skill together, or not at all.
