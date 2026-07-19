# Global cache budget: one base, per-project shards

**Status:** Draft — proposed, not accepted. Two decisions are still open (§10).
**Date:** 2026-07-19
**Supersedes nothing.** Extends the cache-lifecycle design of
[2026-07-05](2026-07-05-cache-lifecycle-redesign-design.md); touches SRS FR-026 and NFR-013.

Every claim below is tagged with how it was established:

| Tag | Meaning |
|---|---|
| **[verified]** | Read from source or observed on disk, anchored to `file:line` |
| **[executed]** | A command or test was actually run and its output observed |
| **[pending]** | Under investigation; do not build on it yet |
| **[inferred]** | Reasoned, not observed. Treat as a hypothesis |

---

## 1. The problem

`importLens.cacheMaxSizeMB` (default 512) presents itself as a global disk cap. It is enforced
**per cache base**, and there is one base per workspace, so the real ceiling is
`N_workspaces × N_editors × 512 MB`. **[verified]**

Measured on the machine that raised this — 5 live bases across two editors: **[executed]**

| Base | Live cache | Cap applied |
|---|---|---|
| `ensurily.code-workspace` (multi-root, 2 project shards) | 12 MB | 512 MB |
| `Desktop/import-lens` | 1.2 MB | 512 MB |
| `nova-dark-qbt-theme` | 1.1 MB | 512 MB |
| Antigravity IDE × 2 workspaces | 100 MB | 512 MB each |

Setting 512 authorises **~2.5 GB**. Nothing is near the limit today, so this has never bitten —
it is a promise the shape cannot keep, not an incident.

### 1.1 What the user asked for

> "if I set 512 mb limit, it will be *caches of all projects combined can't exceed 512 mb*"

That is the intended contract, and it is what this document proposes to deliver.

---

## 2. How this was found

Recorded because the trail explains several near-misses, and because the same reasoning traps are
waiting for the next reader.

1. **The trigger was a red herring.** The VS Code extension panel showed `Cache: 15B`. That figure
   is the host editor measuring `globalStorageUri`, which by design holds only
   `importlens-recycles.json` — the recycle ledger, `{"recycles":[]}`, exactly 15 bytes. **[verified]**
   Import Lens neither computes nor owns that number: `extension/src` contains no directory walk at
   all, and its own formatter emits `"15 B"` with a space. **[verified]** Not a defect. It did,
   however, expose where the caches actually live.
2. **The caches live under workspace storage**, at `<storageUri>/daemon-cache`, per
   [`storagePaths.ts:19-21`](../../../extension/src/daemon/storagePaths.ts). **[verified]**
3. **The budget is per base.** Four independent paths agree, plus a test run: **[verified]**/**[executed]**
   - one daemon per window — `\\.\pipe\import-lens-<pid>-<uuid>`, `first_pipe_instance(true)`
   - one `BudgetCoordinator` per `ProjectCacheRegistry`, one registry per service, rooted at the
     single `storage_path` from Hello
   - `total_shard_file_bytes` → `fs::read_dir(base_path)`, no parent walk
   - the filesystem holds no cross-workspace ledger of any kind
   - `cargo test --test project_cache budget` — 4 passed; the assertion sums one base's shards with
     no cross-base term
4. **"Global" was the right word to remember, and it was never wrong.** The cache-lifecycle design
   defines `global_total = Σ shard.total_bytes` and states *"One source of truth: the shards. No
   separate persisted global ledger to drift."* **[verified]** In that document "global" means
   *across shards*, which is exactly right for a daemon that owns every shard it can see. The word
   leaked onto three user-facing surfaces where a reader cannot know it was scoped to one base.

### 2.1 Interim fix already applied

The commit on branch `fix/cache-budget-scope` corrects the description on all three surfaces
(`package.json`, `README.md` — which ships inside the VSIX — and the SRS settings table), plus one
comment at `project.rs:43` stating the constraint. **That commit is honest but unambitious: it makes
the docs match a behaviour nobody wants.** This document proposes changing the behaviour instead, at
which point those three descriptions change once more.

---

## 3. The feature

**The sum of every project shard's size, across every project and every workspace, is capped by
`importLens.cacheMaxSizeMB`.**

Sharding is unchanged. Read/write cost per project is unchanged. Only the parent directory moves.

```
TODAY — many bases, one budget each
  workspaceStorage/<ws A>/…/daemon-cache/        ← base 1, budget 512 MB
      v1-abc/importlens.redb    project X
      v1-def/importlens.redb    project Y
  workspaceStorage/<ws B>/…/daemon-cache/        ← base 2, budget 512 MB
      v1-ghi/importlens.redb    project Z

PROPOSED — one base, one budget
  <machine-level>/daemon-cache/                  ← one base, budget 512 MB total
      v1-abc/importlens.redb    project X
      v1-def/importlens.redb    project Y
      v1-ghi/importlens.redb    project Z
```

### 3.1 The distinction that makes this safe

**Base** = the folder holding shards. **Shard** = one redb database file for one project.

The design that was migrated away from was a single **monolithic unsharded** `importlens.redb` —
every project's entries in one database. That is what FR-026's sharding requirement exists to
prevent: *"one stable project shard per normalized analysis root … so multi-root windows and
loose-file projects do not share one growing database."* **[verified]**

That rationale is about **sharding**, and sharding survives untouched. Opening project X still opens
`v1-abc/importlens.redb` and nothing else — same file, same entry count, same index, same query
cost. A project never sees another project's entries because they were never in its database.
Moving a folder does not merge databases.

**"Central base + per-project shards" is a third option; the migration history rules out the
monolith, not the shared folder.**

### 3.2 The per-workspace base was never a decision — [verified]

Archaeology settles this, and the answer is stronger than expected:

- **The legacy cache was central base + monolithic file.** At `cb9c9d2^`,
  `storage_path: globalStorageUri.fsPath` with `db_path = storage_path.join("importlens.redb")` —
  one database at the root of a **machine-level** directory. The old FR-026 agreed: *"a `redb`
  database stored in the VS Code global storage directory."*
- **So `cb9c9d2` changed two axes at once** — it introduced sharding *and* relocated the base from
  global to workspace storage — while its commit message justified only the first: *"Move daemon disk
  cache storage to extension-owned per-project shards keyed by analysis root."* The relocation is not
  mentioned at all.
- **FR-026's per-workspace mandate was written 21 seconds after the code** (`cb9c9d2` 05:22:15,
  `d023920` 05:22:36, same session). The SRS *documents* the change; it did not drive it. Only three
  commits in all of history touch `storageUri`.
- **No rationale exists anywhere** — not in the ADRs, known-issues, the decision log, or a comment at
  [`storagePaths.ts:17-21`](../../../extension/src/daemon/storagePaths.ts), which carries none. FR-026's
  sentence about `storageUri` has no justification; the very next sentence, about *sharding*, does.
- **The cache-lifecycle design never mentions where the base lives, and assumes one.** Its
  per-project floor — *"switching to a large project cannot evict a small project's warm set out from
  under the user"* — is a cross-project fairness rule that **only has teeth when many projects share
  one base**.

This does not prove nobody thought about it; it proves nobody wrote a reason down. The requirement
that reads as a Critical constraint is a post-hoc description of an unexplained change, and the
sharding rationale beside it is cleanly separable and untouched by this proposal.

---

## 4. Why not the alternatives

| Option | Verdict |
|---|---|
| **Do nothing, fix the wording** | Already done as an interim, on `fix/cache-budget-scope`. Leaves a cap nobody wants. |
| **Merge all shards into one database** | Rejected. Destroys the sharding rationale — every query would index across entries from projects that are not open. This is the design already abandoned once. |
| **Cross-process byte ledger** (each daemon publishes its base's total to a shared file; all read it) | Rejected as overcomplicated. It keeps N bases and adds persisted derived state that can drift — the exact thing the cache-lifecycle design refused. One base makes the ledger unnecessary: the filesystem *is* the ledger. |
| **Sibling discovery by directory walk** (walk up to `workspaceStorage`, glob for our bases) | Rejected. Reads outside the path the editor granted us, still misses the other editor entirely, and misses the `globalStorage/workspace-cache` fallback base. |
| **One base + per-project shards** | **Proposed.** No new state, no new mechanism, existing evictor works unchanged. |

---

## 5. Why the eviction algorithm does *not* get rewritten

The natural expectation is that a global budget means rewriting eviction. It does not. The evictor is
already written base-relative — "every shard under my base" — with cross-shard LRU, high/low-water
hysteresis, and the per-project floor: **[verified]**

- `total_shard_file_bytes` — `read_dir(base)` + `fs::metadata`, **lock-free, no redb open**
- `collect_shard_targets` — enumerates shards under the base
- `evict_to_budget` — picks victims across shards until under low water

Point these at one base and `Σ all shards ≤ limit` falls out with **no algorithmic change**. The
algorithm was always global; it just had a small world.

**The new work is not the algorithm — it is concurrency around it (§6.1).**

---

## 6. Risks

### 6.1 Multi-process eviction — the one that decides the guarantee

`redb` takes an **OS-level exclusive file lock** per database file; a second process opening the same
shard gets `DatabaseAlreadyOpen`, and [`disk.rs:1113`](../../../daemon/src/cache/disk.rs) then
returns `None`, leaving that shard memory-only. **[verified]**

This splits measurement from eviction, asymmetrically:

- **Measurement stays globally correct.** `fs::metadata` needs no lock, so every daemon sees every
  shard's true size, including shards other windows hold open. **[verified]**
- **Eviction is partial.** `collect_shard_targets` must *open* a shard to trim it. A shard held live
  by another window cannot be opened, and [`disk.rs:611`](../../../daemon/src/cache/disk.rs) returns
  `ShardRollup::empty()` — **zero bytes** — for it. **[verified]**

**The verdict, as the code stands today: `unenforceable_without_coordination`. [verified]** And the
mechanism is worse than "partial eviction" — it evicts *nothing*:

1. The gate at `project.rs:251` uses `total_shard_file_bytes()` — **global physical bytes, siblings
   included**. It decides only *whether* the maintenance pass runs.
2. That number is **never passed into eviction**. `evict_to_budget` recomputes its own total at
   `budget.rs:106` by summing rollups — and a locked sibling shard contributes `ShardRollup::empty()`,
   i.e. **zero**.
3. So the gate fires on global bytes, the evictor sums only what it can open, finds itself under
   budget, and returns at `budget.rs:108` with `evicted_bytes: 0`. **Every 60 seconds, forever.**

Three consequences worth stating plainly:

- **The cap still multiplies by N.** Each daemon permits itself the full budget over its own visible
  subset, so steady-state disk is bounded by `N × cacheMaxSizeMB` — the very thing this feature exists
  to remove.
- **The violation is silent.** `still_over_budget` (`budget.rs:164`) is derived from the same local
  total, so the over-budget warning at `project.rs:264` never fires. A codebase that otherwise
  discloses what it could not do would here be quietly over budget.
- **Visibility is exactly inverted.** The shards a daemon *can* open are the cold, unowned ones; the
  hot, actively-held ones are invisible. In the regime where its visible total *does* exceed budget,
  its victim pool is other projects' cold shards — it strips peers to the 128-entry floor while the
  real consumers are untouchable.

### 6.1.1 Mitigation — localized, not architectural [verified]

Four independent adversarial passes each returned **`real_but_mitigable`**, not a blocker, and
converged on the same three edits — none of which needs a lock, a ledger, or a new process model:

1. **Filter ghosts out of the target set.** `collect_shard_targets` (`project.rs:314-324`) pushes
   every temp-opened shard *regardless of whether the open succeeded* — there is no `disk_available()`
   filter. The predicate already exists five lines away at `project.rs:390`.
2. **Fall back to the `.redb` file size when the open fails.** `ShardTarget` is constructed in both
   arms with the path already in hand, so an unopenable shard can report its true physical bytes via
   `fs::metadata` instead of zero. This is what makes the eviction total honest.
3. **Make the rollup three-valued.** Stop collapsing *unavailable* into *empty*: return
   `Option<ShardRollup>` and fill the unknown from the per-shard JSON sidecar, which is readable
   without taking the redb lock.

With (1)-(3) the evictor's arithmetic matches the gate's, and the cap becomes enforceable for every
shard the daemon can actually trim. It still cannot evict *from* a sibling's live shard — but it can
now see those bytes, stay over budget honestly, disclose it, and reclaim from what it owns.

**No cross-process lock primitive exists in the codebase today** — zero hits for `fs2`, `fd-lock`,
`flock`, `LockFileEx`, or a named mutex. The only OS-level lock in play is redb's own, which is the
cause rather than a tool. **[verified]**

### 6.2 Cleanup ownership — a free service we would be giving up

VS Code reclaims `workspaceStorage` when a workspace entry is pruned and on uninstall. A
machine-level directory is reclaimed by **nobody**. Moving there means owning GC and an uninstall
story that does not exist today. **[verified]** that nothing else cleans it: the only Import Lens
artifacts outside `workspaceStorage` today are two 15-byte recycle ledgers and an empty
`%LOCALAPPDATA%\ImportLens`. **[executed]**

Evidence this matters: **~20 MB is already stranded** under the previous publisher id
(`importlens.import-lens`) across both editors, which nothing will ever reclaim — neither the editor
(wrong extension id) nor the orphan purge (only ever handed the current base). **[executed]**

### 6.3 The `\\?\` prefix — a latent duplicate-shard bug, independent of this work

[`normalize_project_root`](../../../daemon/src/cache/project.rs) normalises separators, trailing
slash, and case, but **does not strip the `\\?\` extended-length prefix**: **[verified]**

```rust
let raw = project_root.to_string_lossy().replace('\\', "/");
let trimmed = raw.trim_end_matches('/').to_owned();
if cfg!(windows) || … { return trimmed.to_ascii_lowercase(); }
```

Extended-length paths demonstrably flow through this system — `rolldown_entry_path` strips `//?/`
explicitly, and they appear in cache keys and diagnostics. **[verified]** So the same project
arriving once as `C:\…` and once as `\\?\C:\…` normalises to `c:/users/…` versus `//?/c:/users/…`,
yielding **two shard directories for one project**.

Whether `project_root` actually arrives in both spellings is **[inferred]**, not confirmed.

Today a duplicate is contained inside one workspace's base. Centralised, duplicates accumulate
machine-wide and every copy counts against the single budget. **This should be verified and fixed
first, on its own merits — it is a latent bug either way, and this feature amplifies it.**

### 6.4 K3 needs re-scoping, not re-deciding

[known-issues K3](../../known-issues.md) accepts a 64-bit FNV-1a shard-id collision as *"negligible
over a user's projects"* — reasoned when a base held one workspace's projects. Centralising widens
the namespace to every project ever opened.

The arithmetic still holds comfortably: 64-bit, ~1000 projects → collision probability ≈ 10⁻¹⁴.
**Not a blocker.** But K3's recorded rationale would no longer match its scope and must be updated,
along with its note that a collision means "a shared eviction budget" — which becomes the point
rather than a side effect.

### 6.5 Four further breakages, all from the same root [verified]

Every one is the lock-invisibility of §6.1 surfacing somewhere else. They are listed separately
because each needs its own fix, and three of them are *user-visible* rather than internal.

| Breakage | Anchor | Effect |
|---|---|---|
| **Manage Cache reports wrong numbers** | `project.rs:520-525`, `:484-488` | `status_for_root` folds the same rollup map, and `list_shards_with_rollups` stamps `entry_count` from it. With a second window open, the UI shows a wrong total and **0 entries** for any shard that window holds. |
| **A project in a second window loses disk cache for the whole session** | `project.rs:390-392` | `cache_for_root` refuses to register a degraded shard so the next call retries — correct when the blocker is a transient in-process temp open, useless when it is another process holding the lock for hours. Result: memory-only, plus a failed open and an un-rate-limited warning **per analysis request**. |
| **"Remove All Caches" can leak an unreclaimable directory** | `project.rs:812`, `:868`, `:340` | `fs::remove_dir_all` deletes entries in readdir order. redb opens through `std::fs::File` without `FILE_SHARE_DELETE`, so on Windows the held `.redb` refuses deletion — but the JSON sidecar beside it may already be gone. That leaves a metadata-less shard directory: invisible to every listing, still counted by the maintenance gate, so the full pass fires forever and the bytes are never reclaimed. |
| **The C5 recency seed silently under-seeds** | `project.rs:174-215` (esp. `:198-203`) | `seed_recency_clock_from_disk` calls `summary_max_seq()` on each shard; a locked one opens disabled and returns **0**. That re-enables the exact inversion C5 was written to prevent — **the actively-used project becomes an eviction victim**. |

### 6.6 What the investigation *confirmed* in the proposal's favour

**Two workspaces sharing one project's shard is correct — "the strongest part of the proposal".
[verified]** `project_cache_shard_id` is FNV-1a over `normalize_project_root(project_root)` and takes
**no workspace, window, or base input**. Two workspaces containing the same project already compute
the *same shard id today*; only the differing base keeps the files apart. Entry identity is
`CacheIdentity`, keyed by package root and entry path. So merging them is a de-duplication, not a
collision — today's split is the accident.

### 6.7 Chores the move creates [verified]

- **The CLI already owns the proposed path.** `cli/importlens.mjs:639-649` resolves
  `%LOCALAPPDATA%\ImportLens\daemon-cache` — introduced by the same commit `cb9c9d2`. Extension and
  CLI would share a base: arguably a feature (one cap covers both), but it must be deliberate.
- **Portable-mode regression.** `globalStorageUri`/`storageUri` relocate into a portable install's
  data folder; a hand-computed `%LOCALAPPDATA%` path does not.
- **Maintenance becomes O(every project ever opened)** rather than O(projects in this workspace), for
  both `collect_shard_targets` and the recency seed.
- **"Remove All Caches" silently widens** from "this workspace" to "every project on the machine".
  A destructive command changing scope needs its confirmation text changed with it.
- **`registry/cache.rs:321-322` is already stale, independent of this work** — it claims *"the
  registry cache is shared across every workspace's daemon via global storage"*, which stopped being
  true when the base moved to workspace storage. Its per-PID temp + rename machinery was built for a
  shared base that the extension no longer uses (the CLI still does).

---

## 7. Plan

Ordered so that each step is independently valuable and independently revertible.

**Phase 0 — prerequisites (ship independently of this feature)**
1. Confirm whether `project_root` reaches `normalize_project_root` with a `\\?\` prefix. If yes, strip
   it there and add a Logic test pinning both spellings to one shard id (§6.3).
2. Land the interim description fix (branch `fix/cache-budget-scope`), so the shipped docs are honest
   regardless of whether this feature proceeds.

3. Correct the stale `registry/cache.rs:321-322` comment (§6.7) — wrong today, regardless.

**Phase 1 — make the evictor honest (ships on its own; no move required)**
4. The three edits of §6.1.1: filter unopenable shards out of the target set, fall back to
   `fs::metadata` for their bytes, and make the rollup three-valued instead of collapsing
   *unavailable* into *empty*.
5. Prove it with a **genuine two-process test** — two daemons, one base — **seen red before it is
   trusted**. The existing race tests only simulate contention inside one process and assert a
   transient hold that heals, which is not the case that matters.

   *This phase is worth doing even if the move is abandoned.* It replaces silent zero-accounting with
   honest accounting, and it is the prerequisite that makes the cap mean anything afterwards.

**Phase 2 — the move**
6. Change `resolveDaemonStoragePaths` to return one base. Keep the `globalStorage/workspace-cache`
   fallback for the no-workspace case.
7. Migration: adopt existing per-workspace shards into the new base rather than stranding them —
   §6.2 shows what stranded caches look like, and §6.6 shows the adoption is a de-duplication.
8. Uninstall/GC ownership for a directory the editor no longer reclaims.

**Phase 3 — the breakages the move surfaces (§6.5)**
9. Manage Cache rollups; the degraded-shard retry (make it recoverable and throttled rather than
   refused); `remove_dir_all` ordering so a failed delete cannot strip the sidecar; and the recency
   seed, so a locked shard does not seed 0 and re-open the C5 inversion.

**Phase 4 — spec and surfaces**
10. Amend FR-026 and NFR-013 (cache base location only — the sharding requirement is untouched, and
    §3.2 shows the location clause never carried a rationale to preserve).
11. Update the three description surfaces to state the now-global contract.
12. Re-scope K3 (§6.4), and rewrite the "Remove All Caches" confirmation, whose scope silently widens
    from one workspace to the whole machine.

---

## 8. Verification

- A **Property** test over several shards in one base asserting Σ sizes ≤ budget after a maintenance
  pass.
- A **two-process** test — genuinely two daemons, one base — asserting the documented guarantee,
  whichever §6.1 selects. This is the test that must be **seen red before it is trusted**: the
  existing race tests only simulate contention *within* one process and assert a transient hold that
  heals, which is not the case that matters here. **[verified]**
- A **Drift** test pinning the settings description against the enforced scope, if a machine-readable
  form of that scope exists. If it does not, prefer deleting the second source over testing it.
- `ANALYZER_REVISION` does **not** move: this changes where entries live, not what any number means.

---

## 9. Investigation results

Three parallel investigations plus four adversarial verification passes, all completed. Findings are
folded into §3.2, §6.1, §6.5-6.7 above. Summary of verdicts:

| Question | Verdict |
|---|---|
| Was the per-workspace base deliberate? | **`incidental`** — no rationale recorded anywhere; the SRS clause post-dates the code by 21 seconds (§3.2) |
| Was the legacy cache a single unsharded file? | **Confirmed yes**, at a *central* base — so the migration argues *for* this proposal, not against it |
| Is the cap enforceable under one shared base, as the code stands? | **`unenforceable_without_coordination`** — and it evicts nothing at all, silently (§6.1) |
| Is that a blocker? | **No.** All four adversarial passes returned `real_but_mitigable`, converging on three localized edits (§6.1.1) |
| Is sharing one shard between two workspaces correct? | **Yes** — shard identity has no workspace input; today's split is the accident (§6.6) |

### 9.1 Still genuinely unknown

- Whether the author consciously considered and rejected a central base for the new shards. Absence
  of a written reason is not absence of thought — only absence of evidence.
- Whether two windows opening the **same folder** receive the same `storageUri`. If they do, the
  per-workspace base does not actually prevent cross-window shard collisions today, which would
  remove the strongest *unstated* justification for it. **Not verified.**
- Whether `project_root` genuinely reaches `normalize_project_root` in both `C:\…` and `\\?\C:\…`
  spellings (§6.3). Still **[inferred]**.
- Every multi-process claim above is read from source. **Nothing was executed against two live
  daemons** — which is exactly why Phase 1 requires that experiment before the fix is trusted.

---

## 10. Decisions required

1. **Where does the single base live?**
   - `%LOCALAPPDATA%\ImportLens\daemon-cache` — genuinely machine-wide, already an Import Lens
     location (the CLI writes there), and covers both editors plus the CLI with one cap.
   - Each editor's `globalStorage` — simpler and stays inside editor-managed storage (§6.2 largely
     evaporates), but VS Code and Antigravity would still get 512 MB each. On the reporting machine
     that is 1 GB, not 512 MB — it does not deliver the asked-for contract.
2. **Is Phase 1 worth doing on its own?** The §6.1.1 edits make the evictor's accounting honest and
   do not require the move. If the move is deferred or dropped, they still fix silent
   zero-accounting. Recommend yes, independently.

3. **What contract does the setting advertise?** With Phase 1 the honest wording is:
   *"the combined size of all project caches, capped at this value; bytes held live by another open
   window are counted and disclosed but cannot be reclaimed until that window releases them."*
   That is a real global cap with one stated exception — not the unqualified guarantee, and not
   today's silent N × multiplication. Confirm this is the contract to build toward before Phase 2.

---

## Appendix: unrelated findings surfaced along the way

Recorded so they are not lost; none is part of this feature.

- **~20 MB stranded** under the previous publisher id across both editors, reclaimable by nothing.
  Deleting it is safe and needs no code. **[executed]**
- **Multi-window cache degradation, today.** Two windows on the *same* workspace already share a base
  and collide on shards: the second window's shard goes memory-only and does not heal while the first
  holds it, with an un-rate-limited warning per request. Narrow, and by the repo's bar neither a wrong
  number nor a wedge. **[verified]**
- **`remove_shard` deletes unconditionally** ([`project.rs:812`](../../../daemon/src/cache/project.rs)),
  with no cross-process check. On Windows an open handle should make this fail benignly; on unix it
  would unlink a live sibling's inode. Windows-first product, so low priority — but it becomes
  reachable far more often under a shared base. **[inferred]** for the platform behaviour.
