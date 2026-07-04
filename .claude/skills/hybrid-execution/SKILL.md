---
name: hybrid-execution
description: >-
  Decide HOW to execute a multi-commit or multi-file piece of work for the most
  quality per token — stay inline, spawn an independent reviewer, parallelize, or
  escalate. Use when about to execute a multi-step implementation plan, build a
  feature across several steps or commits, do a non-trivial refactor or migration, or run
  a broad multi-file audit/review — or when weighing "should I use subagents / a
  workflow, or just do it inline?". Skip simple single-file or one-shot tasks a
  single pass handles.
---

# Hybrid Execution

Coding is tightly interdependent — the case multi-agent handles *worst* — so
**implement inline** and spend independent-agent tokens only where they buy quality
— mainly a **cheap independent review at the risky moments**, not a fleet.

## Decide with three questions

1. **Interdependent or independent?** Edits that reference each other or need each
   other's results → **inline, one context**. Strands that share no code and no
   ordering (8 unrelated adapters, N places to search) → **parallelize**, one worker
   each. In doubt, inline — a wrong split costs more than a serial pass.
2. **Risky?** Changes a public API/protocol, a load-bearing invariant, concurrency
   or `unsafe`, or a data/cache format — **or** you can't fully eyeball the diff →
   **independent review** by a fresh context that did NOT write it. Additive code
   covered by tests, renames, docs, config → **no review**.
3. **Does it flood your context with tokens the result doesn't need?** A big audit,
   log trawl, or unfamiliar-API survey → hand it to a **read-only subagent that
   returns a distilled summary** — whether or not it's parallelizable; the point is
   keeping the implementer's context clean for the code, independent of serial-vs-
   parallel.

## Default recipe

1. Plan the work as logically-coherent commits.
2. **Implement inline**, reusing your loaded context; narrow check per step, full
   gate per commit.
3. **Review the risky commits** (question 2) with a fresh subagent — the default,
   don't stop to ask. Stage the change, hand the reviewer `git diff --staged` + the
   plan; it is **read-only and reports only**. Its findings are **hypotheses**:
   verify each against the code (reproduce or refute), fix what you confirm, decline
   the rest with a one-line reason. An unverified "fix" adds bugs; a performative one
   hides the issue as much as silence.
4. **Fan out** genuinely independent sub-steps; barrier, collect, continue inline.
5. **Verify proportionally** at the end — drive the changed behavior for real if it
   has runtime surface; build + tests suffice for mechanical commits.

## Why it stays cheap (the levers)

- The implementer stays inline — you pay for context once, not once per agent.
- The reviewer gets **diff + plan** (it may open the touched files), never the whole
  task. An independent *perspective* is what adds quality, and that's cheap to get.
- **Match model/effort to the strand**: mechanical fan-out workers on a cheap model
  at low effort; the risky-diff reviewer on the strong one. That's where
  quality-per-token peaks.

## Anti-patterns

- Parallel agents on interdependent code — serial latency + merge pain, no shared
  context.
- The author reviewing its own work — the value is *independence*.
- A subagent rebuilding context to *write* the core — full cost, no benefit.
  (Offloading bulk *reads* is the opposite; do that.)
- Over-scaling — a reviewer/approver/developer trio for a two-line change.

## Rarer cases (skip on routine work)

- **Parallel workers that mutate files** → give each its own **git worktree**, merge
  at the barrier; prompt-level file boundaries don't protect `target/`, `dist/`, the
  lockfile, or the git index.
- **Voting** — for a claim that is high-stakes *and* genuinely uncertain: K=3
  independent verifiers, each told to refute; clear on 2 of 3.
- **Orchestrator-workers** — a lead splitting an *unknown* set of subtasks; only for
  large migrations/audits where the value clearly justifies the multiplied cost.
- **Change cadence** only for exceptional jobs — every-commit review for something
  safety-critical, once-at-end for something throwaway.
- **No subagent tools?** Run the same decisions inline; at each risky boundary do a
  **structured self-review** over `git diff` alone — read it adversarially and list
  findings before fixing — and state plainly that independent review wasn't
  available, so the bar is lower than a true second set of eyes.

## Why this shape

Anthropic measured multi-agent research at ~15× the tokens of a chat and a single
agent at ~4× — worth it for independent parallel research, wasteful for
interdependent coding. Hence: inline implementation, independent review only where
risk lives. In Claude Code the subagent/reviewer/worker is the `Agent` tool (let it
read `git diff` itself); structured fan-out at scale is `Workflow`.
