---
name: lean-orchestration
description: Use when starting any non-trivial task — a feature, bug hunt, review/audit, design critique, refactor, or mixed prompt — before dispatching any subagent or writing a plan. NOT for quick lookups, tight debug loops, or single-file changes — EXCEPT a single-file change that can alter a user-visible number or wedge/lose state, which does route here.
---

# Lean Orchestration

**Goal: highest-quality output for the least token spend.** Quality comes from adversarial *framing* — refute-first prompts and falsifiable-claim bars, which are free text — plus *fresh context*, which is NOT free: every fresh context is a dispatch that re-pays its fixed overhead and its read. Spend therefore follows the number of dispatches; triage, the ladder, and the budgets below keep that number small. Maximum framing, fewest fresh contexts.

## Core principle — the escalation ladder

Every check has a cost. Climb only as far as the stakes force you — and the rungs are entry depths, not a sequence: enter at the stakes-matched rung.

```
1. Gates (tests / typecheck / lint)   ← deterministic, ~free. ALWAYS first.
2. One adversarial skeptic             ← cheap. For contested / non-obvious claims.
3. Small panel (2-3 diverse verifiers) ← expensive. ONLY for high blast-radius / auto-applied changes.
4. Proof-burden pass (critical only)   ← extra pass, run by skeptic-max. Finding presumed at its Step 7 default; flips only on PROOF.
```

Never pay an LLM to find what a gate finds for free.

**Second lever — tier the effort, never the model.** Do not downgrade the model to save cost: a smaller model spends more tokens for worse output on the same task, netting a higher real cost. Tier by reasoning effort instead — effort is frontmatter-only (there is no per-dispatch knob), so dispatch `navigator-lite` for fully-specified mechanical lookups and full `navigator` for anything needing synthesis. Review and verification roles (`finder`, `skeptic`) have no lite tier by design: there is no "mechanical" review, and a low-effort skeptic rubber-stamps. The only special verification tier points *up*: `skeptic-max` (effort: max) for Step 7's proof-burden pass. Verify a low-effort worker's green report yourself — cheap workers are confidently wrong more often.

**Third lever — dispatch to lean role agents.** A subagent pays a fixed context tax before it reads its prompt, and the dispatch call can't slim it — only the agent definition can. Use the lean roles (`navigator`, `finder`, `skeptic` — few tools, MCP stripped); reserve `general-purpose` (full tool surface) for workers that genuinely need broad tools. Know what a subagent inherits: custom agents always get project CLAUDE.md (never re-paste it; built-in Explore/Plan skip it), while skills content and session-hook context never arrive (never assume they're there — inject the distilled grounding instead).

## Step 0 — Anti-trigger (check FIRST, before anything)

Go straight inline, no orchestration, for:
- single-file changes — unless the change can alter a user-visible number or wedge/lose data/state; stakes override size: after gates, spend one skeptic on it
- quick lookups / factual questions
- tight debug loops where you need the raw bytes in hand

The routing overhead below only pays off when exploration is broad/deep enough that reading it inline would bloat context, or when correctness stakes justify verification. If in doubt on a *small* task, stay inline.

## Step 1 — Route (classify, then state it in one visible line)

Classify the input along four axes. These reconfigure every downstream step:

| Axis | Values | Controls |
|---|---|---|
| **Deliverable** | answer / report / fixes / feature / refactor | whether to implement; triage mode |
| **Review objects** | code / design-artifact / spec | which lenses; what to ground |
| **Scope** | repo / branch-diff / module / file | fan-out size; partitioning |
| **Stakes** | reversible note ↔ auto-applied edit | verify depth |

Emit one line, e.g.: `Route: review of branch diff (code + design), deliverable=report → ground, 2 lenses, verify contested only, no implement.` This lets the human veto a misroute *before* cost is spent — routing has no other upfront check. Mid-flight, the route is correctable: when evidence contradicts it (a "fix" turns out to need a design change), emit a corrected Route line and switch paths — don't ride a misroute to the end.

The routing questions that catch the most misfires: *Is there a design/spec artifact to critique? Is the deliverable a report or fixes? Is it diff-scoped?*

**Path shape by deliverable** (which steps fire — the rest are skipped):

| Deliverable | Path | Floor (min dispatches) |
|---|---|---|
| **answer** (question/research) | Route → Ground → Navigate → answer inline. No Find / Triage / Implement. | 0-2 (ground? + navigator) |
| **report** (review/audit/critique) | Route → Ground → Find → Dedup → Triage(rank+label) → Verify contested → deliver doc. No Implement. | ~3-5 budgeted finders + skeptic |
| **fixes** (bug/improvement) | as report through Verify-contested (that step is NOT skipped), plus **Clarify if ambiguous**; Triage is a filter → Implement → Verify-impl loop. **Known defects (already named in the prompt) skip Find entirely:** Navigate to root cause → Triage(filter) onward — never pay finders to rediscover a stated bug. | hunt: as report; known defect: 1-2 (navigate + impl skeptic) |
| **feature** (build X) | Route → Ground → **Clarify** → Navigate → Plan → Implement → Verify-impl loop. No Find. | 1-2 (navigator? + verify skeptic) |
| **refactor** (reshape, preserve behavior) | Route → Ground (only if a spec / won't-fix registry bears on the touched behavior) → Navigate → Implement → Verify-impl with the verify question flipped to **behavior preservation** (Step 10). No Find; Clarify only if the target shape is ambiguous. | 0-2 (preservation check; zero when tests carry it) |

The floors are honest: ~inline cost holds for answer / feature / refactor / known-defect fixes; report-and-hunt routes deliberately cost a lens-multiple of one inline read — partitioned coverage plus fresh-context adversarial review no inline pass can produce, with the budget capping how much. On *small* tasks pure inline is cheaper and better — that is what Step 0 protects (stakes hatch aside). Apply deliberately, not reflexively.

## Step 2 — Ground (load shared context ONCE, inject into workers)

If the task references a spec, README, design image, "our scope/usecase," or a won't-fix registry (e.g. `known-issues.md`): load and distill it **once** — this is itself a navigate task (see Step 3). Inject the *distilled* summary (+ anchors) into every finder/worker prompt. Never let N workers each re-read the same sources (N× cost) or vision-parse the same image N times.

Keep the grounding summary compact and anchor-backed so workers pull raw detail only if a specific finding needs it. Ground as a *separate dispatch* only when more than one worker will consume it AND the source is big; for a single worker or a small source, fold the grounding into that worker's prompt or read it inline — a distillation nobody shares is pure overhead.

## Step 2.5 — Clarify before building (gate; feature, ambiguous fixes & ambiguous refactors)

Building on an underspecified request is the most expensive failure — you implement, verify, then learn it was the wrong thing and throw it away. Resolve any **material unknown that would change the design or cause rework if guessed wrong.** Graduated, cheapest first:

1. **State the assumption and proceed** (free) — minor ambiguity; the user vetoes if wrong.
2. **Ask 1-2 targeted questions** — a couple of load-bearing unknowns.
3. **Light grill** — invoke `grilling` (model-invocable) for a plan whose critical assumptions must hold but that doesn't warrant a written record.
4. **Full grill with docs** — for a real feature or a consequential design: **launch the interview yourself — don't wait to be asked.** `grill-with-docs` itself is user-launch-only (`disable-model-invocation` — do NOT edit that skill), so invoke its components directly: `grilling` + `domain-modeling` together. Same interview; it captures ADRs + glossary as it goes, so its output *is* the assumptions ledger — no separate record needed. (The user answers the questions; "auto" means you start the grill, not that you answer for them.)

Gate on *actual* ambiguity, not reflexively — a crisp request skips this entirely (anti-trigger philosophy). **This step owns the call/no-call decision:** the grill launches only when this gate says so — never reflexively because the grill skills exist, never for a crisp request, and never on routes that skip Step 2.5. (A user-typed `/grilling` or `/grill-with-docs` overrides — that is their call, not this gate's.) **The grill always runs in the main agent, never inside a subagent:** a subagent has no channel to the user, so it would either stall or — worse — answer the questions itself and bake hallucinated "decisions" into the plan. (The lean role agents cannot invoke skills at all, by design.) **Stop condition:** grill until the load-bearing assumptions are pinned, then stop; don't grill nits.

Every choice made via rung 1 (state-and-proceed) goes into a short **visible assumptions list** — one line each. If a Step 10 strike later appears, check this list first: a false assumption turns a mystery bug into a one-line correction. (Rung 4 doesn't need this — its ADRs already record the decisions. Rung 3 DOES: plain `grilling` writes nothing, so its outcomes go on the list too.)

*Why this is a least-uses win, not a tax:* an ungrilled plan is what feeds the Step 10 strike loop — repeated implementation issues usually mean the plan was never pinned. Clarifying once upfront is far cheaper than N verify-fix rounds and a possible throwaway.

## Step 3 — Explore / Navigate (delegate read-to-understand)

Discriminator: **am I going to *edit* these files, or just *understand* them?**
- **Read-to-edit → inline.** You need the exact bytes anyway (Edit matches against them).
- **Read-to-understand → delegate** to a navigator subagent; the bytes are disposable.

Before any dispatch, check the answer isn't already in hand — the grounding summary, an earlier worker's return, session memory, an existing doc. Re-deriving a known answer is pure waste. Then dispatch a **precise question**, never "explore module X" (vague scope guarantees a re-dispatch). Navigators are a **budget, not a concurrency cap: ~3-4 dispatches per route, total** — exceed only with a stated reason; only genuinely independent scopes. If two scopes share a seam, one worker owns the seam. Dispatches run in the background — fire them first and keep working inline while they run. Prefer the `navigator` agent type — it is tuned to the return contract below. (For a broad locate-only sweep where no contract is needed, the built-in `Explore` agent is cheaper still — it skips CLAUDE.md.) A lookup satisfiable by one grep or one Read stays inline (Step 0); a *mechanical multi-file* lookup runs in a sandboxed exec tool when one is available (e.g. context-mode's `ctx_batch_execute`) — zero dispatch tax: no agent system prompt, no inherited CLAUDE.md; `navigator-lite` is for fully-specified *searches* where discovery takes several steps but the question is mechanical.

**Navigator return contract:**
```
FINDING:  the answer to the precise question asked
ANCHORS:  path:line for every claim (planner pulls exact bytes on demand)
VERBATIM: load-bearing lines only — signatures, the specific branch, the type def. Nothing else.
UNKNOWNS: what I did NOT cover / where I'd be guessing
```
`UNKNOWNS` prevents the expensive failure (a second round + latency) by letting the planner decide up front whether one navigator was enough.

## Step 4 — Find (review/audit tasks; skip for pure feature builds)

Run finder lenses over the **partitioned** scope (prefer the `finder` agent type — one lens + one slice per dispatch). Two different things move differently: the **grounding summary** (Step 2) is small and shared → broadcast to every finder; the **review surface** (the diff/modules) is large → partition it, assign each finder a slice, do NOT hand the whole surface to every finder (that's N× the read). **Fan-out is a budget, not a concurrency cap: ~3-5 finder dispatches per review, total** (a concurrency cap merely queues waves; a budget bounds spend). Prefer fewer, larger slices; at module-or-smaller scope one finder may run ALL applicable lenses in a single dispatch — lens-per-dispatch is for large surfaces. Lenses do not all run everywhere — code-defect covers every slice; spec-conformance runs only where the grounding maps a spec claim onto the slice; design-critique runs ONCE over the architecture surface (the seams: module boundaries, signatures, shared types — NOT a re-read of the full diff). If the budget drops a lens×slice pair, say so in the deliverable — silent truncation reads as "covered everything." Exceed the budget only when the user explicitly asks for exhaustive coverage. Lenses to include as the objects demand:
- **code-defect** (correctness / perf / stability) — acceptance bar: a **falsifiable claim** (inputs → wrong output). No failing case ⇒ it's a *note*, not a bug.
- **spec-conformance / drift** — compares implementation to spec + design to catch **missing features**. An absence is unfindable by reading code alone; it requires Step 2's grounding.
- **design-critique** — acceptance bar is different: judged by "is this a real architectural risk *given our scope*," **no falsifiable-input requirement.** Do not demote real design flaws just because they lack a failing input.

## Step 5 — Dedup

Collapse findings to root cause, **across lenses** (a perf issue that is also a design flaw is one finding). One finder often reports the same root in three files — verifying it three times is 3× waste.

## Step 6 — Triage (behavior depends on the deliverable)

- **Deliverable = fixes** → triage is a **filter**: fix-now only what shows a wrong result or can wedge/lose data/state; everything else → notes, back in the queue.
- **Deliverable = report** → triage is a **rank + label**: keep everything, rank by severity, label `fix-now / note / won't-fix`. Do NOT silently drop low-severity findings the user asked to see.
- In both: **filter against the won't-fix registry** (`known-issues.md`) so deliberately-unfixed decisions are not reparaded.

Triage is the master cost lever — the cheapest verifier is the finding you decided not to chase.

## Step 7 — Verify findings (adversarial; scaled)

Verify only **contested / high-stakes** survivors — a blatant defect with an obvious failing input rides on the fix + gates.
- **Framing is always on** (free): fresh context, prompted to *refute*, default to "not real" under uncertainty. Same agent that found it must not verify it. Prefer the `skeptic` agent type, and state the finding's lens in the dispatch — the skeptic applies a per-lens evidence bar (a design risk is never refuted merely for lacking a failing input).
- **Reject-method — an extra, stricter pass for critical / suspicious findings only** (it costs a pass, so reserve it; dispatch `skeptic-max` — same contract at maximum effort, and the dispatch must state the burden direction below). Findings already triaged critical go STRAIGHT to `skeptic-max` — never pay a plain skeptic first for a finding whose stakes you already know. The finding is **presumed decided by default** and flips only on positive **proof** — a concrete repro/trace — not merely "un-refuted." Which default, and thus where the burden of proof sits, depends on which error is costlier:
  - **Fix is risky / expensive, impact-if-unfixed tolerable → default-reject:** prove it's *real* before touching code (stops phantom fixes from breaking working code).
  - **Impact-if-real is catastrophic (data loss, security, wedge) → default-suspect:** prove it's *safe* before dismissing (a rejected-but-real safety bug beats a confirmed non-bug in cost). *The burden of proof falls on whichever side is cheaper to be wrong about.*
- **Depth scales with the consumer AND the stakes:** report a human will review → one skeptic, no panels (human is the backstop). Auto-applied edit → panel per blast radius.
- **Batch small survivors:** several related low-stakes findings share one skeptic dispatch (one context load, one pass). One-verifier-per-finding is for contested or critical items — independence matters across verifiers of the *same* finding, not across findings. Soft cap ~5 findings per skeptic — past that, attention dilutes; split the batch.
- Run **independent** verifications in **parallel** (same tokens, less latency). Serialise only to **early-exit** (first verifier reveals the finder hallucinated → stop) or when findings interact.

## Step 8 — Plan / Synthesize (inline)

Do the thinking, decisions, and merge inline using anchors; pull exact bytes on demand. On merge, run a **seam-check**: reconcile shared types / call signatures across worker outputs rather than stapling summaries together (boundary drift is the failure mode).

**Turn the reject-method inward before committing to a build.** A wrong plan costs more than a wrong finding — so subject the plan's *load-bearing* assumptions to the same proof-burden (Step 7): presume each is wrong and require a reason it holds. The rung-4 grill (`grilling` + `domain-modeling`) does this interactively for features; for lighter work, do it inline. Don't ship a plan whose critical assumptions only survived because nobody attacked them. For state/lifecycle/caching/failure designs, also trace one value end-to-end (one navigator question) before committing — the expensive gaps are *unstated* assumptions no attack list contains.

## Step 9 — Implement (inline by default)

- **Inline** for anything sequential or needing judgment / a debug loop.
- **Delegate only parallel-independent slices** (separate worktrees) — implementation delegation buys *isolation*, not context savings.
- After any implementation subagent, **read `git diff` inline** before proceeding — otherwise your mental model diverges from disk — and check the worker didn't weaken a spec or test to make its claim pass: fix the code, never narrow the claim.

## Step 10 — Verify implementation (loop with a written stop)

- **Gates first** (deterministic, free); spend a skeptic only on what gates can't see (logic correctness, spec conformance). A gate *added in this task* must be seen red once before its green counts.
- **Refactor routes flip the verify question to behavior preservation:** gates green before AND after, and any skeptic hunts for behavior *changes* ("prove old ≡ new"), not defects — a defect-hunter blesses a reasonable-looking changed number. Verdict semantics invert here: a PROVEN block = a proven behavior *change* (preservation disproven); SURVIVES = "old ≡ new" held under attack.
- Panel size scales with blast radius; verifier scope is bounded to **correctness + spec, not taste** (nits are logged, not looped — else the loop never terminates).
- **Strike rule:** the counter keys on **recurrence**, not raw count. Three *different, unrelated* fixed issues is thorough review working. **Same class ×3, or fixes that breed new issues (whack-a-mole) → stop patching and interrogate the plan/design.** The main agent holds this state, labeling each round's issues to detect recurrence.

## Step 11 — Deliver

Report path + a compact summary. Artifacts (docs, plans, findings) go to files, not the chat. Tag each reported finding with its **verification level** (gate-caught / refuted-survived / proof-confirmed / finder-claim / refuted) so the reader knows where to spend their own attention. `finder-claim` = reported but never independently verified — most report-mode findings are this; tagging them a level up overstates confidence. `refuted` = adjudicated false — in a report deliverable it stays, marked as such, never silently dropped.

## Loop stop conditions (name the exit before entering any loop)

- verify-findings: all survivors adjudicated → done.
- verify-implementation: clean on correctness/spec, OR strike-limit → escalate to design review / human.
- No open-ended "keep going until sure."
