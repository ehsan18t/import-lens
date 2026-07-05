# Cache SWR + D3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax. **Before executing, run a fresh planning-brief + verification pass** (as done for Plans 1–2): this plan predates execution and the exact `file:line` anchors below must be re-confirmed against HEAD.

**Goal:** Serve the last-known bundle size *instantly* (flagged stale) while recomputing in the background, instead of dropping stale entries and showing a loading state — with a data-layer freshness flag, a transient→`Unverified` graduation, a first-party TTL bypass (D3), and a CI/CLI force-fresh escape hatch.

**Architecture:** Plan 3 of the cache-lifecycle redesign (spec §4.3/§4.4/§4.5/§4.3.1), building on Plans 1–2 (freshness core + identity v4, merged). The daemon's `ImportCache::get` currently *evicts* on `Stale`; SWR flips that to serve-stale-then-revalidate. The load-bearing new work is **delivery of the refreshed value**: the analyze/file-size request answers exactly once and its channel closes, so a background refresh must push a *new* `ServerOutboundMessage` variant (mirroring the registry-hint refresh already in `ipc/server.rs`) that a new extension client handler writes straight into `AnalysisStore`. The freshness flag, `Unverified` graduation, and D3 first-party bypass are cheap and localized.

**Tech Stack:** Rust (daemon: tokio + rayon + papaya), TypeScript (extension), the existing length-prefixed IPC protocol.

> **HEAD re-anchor (validated 2026-07-06 against `cf9945c`, branch `redesign/cache-lifecycle`).** All anchors below still exist; corrections to apply before executing:
> - **Naming (Plan 2):** the cache identity struct is now the unversioned `CacheIdentity` (with a `CACHE_KEY_VERSION: u32 = 4` const deriving the `v4:` prefix), **not** `CacheIdentityV4`. This doc already refers to `CacheIdentity` — keep it that way.
> - **Task 1 anchor is correct as written.** `fingerprints_with_content_hashes` **exists** at `daemon/src/pipeline/graph.rs:353` and is the shared value-side builder; `service::dependency_fingerprints` (`service.rs:1595`) already routes the graph case through it, so fixing that one function closes the window for both. *Optional simplification:* `content_len` is already available as `ModuleRecord.original_source_bytes` (`= source.len()`, equals `fs::metadata().len()` for the raw UTF-8 read) — only the **read-time mtime** is genuinely a new field.
> - **Task 2:** `ImportResult` (`protocol.rs:118`) does **NOT** derive `Default`, and there is no `..Default::default()` construction site today. Every `ImportResult { .. }` literal must set `freshness:` **explicitly**. Confirmed non-test sites: `pipeline/analyze.rs:213,335,432,481,694`, `pipeline/types_only.rs:36`, `service.rs:1705`; test helpers: `report/model.rs:410`, `tests/cache_disk.rs:69`, `tests/memory_cache.rs:5`, `tests/freshness_core.rs:74`, `tests/project_cache.rs:26`.
> - **Task 4:** `Stale` and `Gone` currently **share one match arm** (`memory.rs:127`: `Freshness::Stale | Freshness::Gone =>`). The SWR flip must **split** them — `Stale`→serve-stale, `Gone`→still evict.
> - **Task 6 (sharpest hazard):** the `RefreshedResults` push is **unsolicited** — the analyze/file-size request already resolved and its pending entry was deleted — so it **cannot** reuse the registry-hint `onPartial` path (that returns early when no pending entry keyed by `request_id` exists, `client.ts:365`). It needs a **standalone message-type-dispatched handler** keyed by document+workspace, routed straight to `AnalysisStore.applyRefreshedResults`.
> - **Load-bearing risk (unchanged):** Task 5's second server→client push is the novel work; every daemon-side mirror anchor is accurate (`ServerOutboundMessage` at `ipc/server.rs:38`, drainer select at `server.rs:152`, `RefreshRegistryHints` precedent `server.rs:~521-593`, mirror test `tests/server.rs:809`).

## Global Constraints

- **Conventional Commits, mandatory body, header ≤ 72 chars** (a `commit-msg` hook enforces both — keep subjects short).
- **Gates before each commit:** `cargo clippy --workspace --all-targets` (lint = `deny`), `cargo deny check`, `cargo fmt`; extension: `pnpm --filter extension typecheck` + Biome. This env's rust-analyzer shows **stale** errors mid-edit — trust `cargo check -p import-lens-daemon --all-targets` / `cargo test`, never the editor.
- **`ImportResult` is serialized to the disk cache** (inside `CachedImport`): any new field MUST be `#[serde(default)]` (and `skip_serializing_if` where sensible) so old on-disk entries still decode. Do NOT bump the disk schema for this.
- **Protocol compatibility:** the extension `protocol.ts` version and the daemon protocol must stay in lockstep; new outbound message variants are additive and gated on capability, never breaking an older peer.
- **No UI this iteration:** the freshness flag lives in the *data* layer only (spec §4.5). Do not add badges/decorations — just carry and store the flag.
- **SWR must never serve stale in CI:** the `importlens check` path forces synchronous fresh computation.
- **`file:` gone vs changed:** SWR serves stale only when a dep *changed and still exists*; a `Gone` (`NotFound`) dep must still drop + recompute (never serve stale — recompute can't succeed).

## File Structure

- `daemon/src/pipeline/graph.rs` — `ModuleRecord` carries read-time `content_len`/`content_mtime_millis`; `fingerprints_with_content_hashes` uses them (Task 1).
- `daemon/src/ipc/protocol.rs` — `ResultFreshness` enum + `ImportResult.freshness` field; a new `RefreshedResults` outbound message + a `force_fresh` request flag.
- `daemon/src/cache/key.rs` — a `cache_key_is_first_party(key) -> bool` helper (D3).
- `daemon/src/cache/memory.rs` — freshness-aware get; in-flight dedupe set; the `Stale`→serve-stale flip; D3 fast-path gate; `Unknown`→`Unverified`.
- `daemon/src/service.rs` — `analyze_with_cache` returns/propagates freshness; spawns revalidation; `force_fresh` bypass.
- `daemon/src/ipc/server.rs` — a `ServerOutboundMessage::RefreshedResults` variant + drainer, mirroring registry refresh; wire the revalidation push.
- `extension/src/ipc/protocol.ts`, `extension/src/ipc/client.ts`, `extension/src/analysis/state.ts` — the new outbound handler that writes the store.
- `cli/importlens.mjs` — set `force_fresh: true`.

---

## Task 1: Harden fingerprint capture to read-time (close the after-analysis window)

**Why:** the pre-SWR integrity review found a residual TOCTOU: `dependency_fingerprints` re-stats each file *after* analysis, so a dep changed between graph-build and the stat is stored as `{new_len, new_mtime, old_hash}`; `check_fingerprint`'s mtime+len pre-filter then returns `Fresh` without consulting the hash. SWR leans on freshness accuracy, so close this first.

**Files:**
- Modify: `daemon/src/pipeline/graph.rs` (`ModuleRecord`, `load_module_from`, `fingerprints_with_content_hashes`)
- Test: `daemon/tests/freshness_core.rs`

**Interfaces:**
- Produces: `ModuleRecord.content_len: u64`, `ModuleRecord.content_mtime_millis: u64` (captured at read time, alongside `content_hash`); `fingerprints_with_content_hashes` builds each module's `FileFingerprint` from the read-time `{len, mtime, hash}` instead of an after-analysis stat.

- [ ] **Step 1: Failing test** — a dep whose length is unchanged but content differs, captured such that the stored fingerprint's mtime/len match the current file yet the hash differs → `check_fingerprints` must return `Stale` (not `Fresh` via the pre-filter). Build it by asserting that `fingerprints_with_content_hashes` produces a fingerprint whose `modified_millis`/`len` equal the values captured *at graph-build time* (compare to a `fs::metadata` taken before a post-build touch), and that a post-build touch to the file does not retroactively change the stored fingerprint.
- [ ] **Step 2:** Run it — FAIL (currently the stat is taken after build, so a post-build touch is reflected).
- [ ] **Step 3:** In `load_module_from`, right where `content_hash` is computed from the raw `source` (after `fs::read_to_string`), also capture `content_len = source.len() as u64` and a read-time mtime from a `fs::metadata(&path)` taken at that moment; store all three on the `ModuleRecord` (add the two fields).
- [ ] **Step 4:** In `fingerprints_with_content_hashes`, for a path that IS a graph module, build its `FileFingerprint { path, len: module.content_len, modified_millis: module.content_mtime_millis, content_hash: Some(module.content_hash) }` directly (no `file_fingerprint_with_hash` stat). For non-module paths (manifest / `dependency_paths` not loaded as modules), keep the existing stat fallback (`content_hash: None`). Preserve sort+dedup-by-path.
- [ ] **Step 5:** Run the test + `cargo test -p import-lens-daemon` green; clippy.
- [ ] **Step 6:** Commit `fix(daemon): fingerprint deps at read time, not post-analysis`.

---

## Task 2: `ResultFreshness` data-layer flag

**Files:**
- Modify: `daemon/src/ipc/protocol.rs` (enum + `ImportResult` field)
- Modify: `extension/src/ipc/protocol.ts` (mirror)
- Test: `daemon/tests/` (serde round-trip incl. old-entry default) + a tsc check

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
  #[serde(tag = "kind", rename_all = "snake_case")]
  pub enum ResultFreshness {
      #[default]
      Fresh,
      Stale { revalidating: bool },
      Unverified { reason: String },
  }
  ```
  and on `ImportResult`: `#[serde(default)] pub freshness: ResultFreshness,` (added as the last field).

- [ ] **Step 1: Failing test** — decode an old-format `ImportResult` JSON/msgpack *without* a `freshness` field and assert it defaults to `Fresh`; encode a `Stale{revalidating:true}` and round-trip it. (This guards the `#[serde(default)]` disk-compat requirement.)
- [ ] **Step 2:** Run — FAIL (no field/enum).
- [ ] **Step 3:** Add the enum + field. `ImportResult` does **not** derive `Default` (and nothing uses `..Default::default()`), so update EVERY `ImportResult { .. }` literal to set `freshness: ResultFreshness::Fresh` **explicitly** — see the confirmed site list in the HEAD re-anchor note above; grep `ImportResult {` to catch any new ones. Default-stamp is `Fresh`. (Alternatively, add `#[derive(Default)]`-compatible defaults, but the field already carries `#[serde(default)]` for disk-decode; the in-code literals still need the explicit value.)
- [ ] **Step 4:** In `extension/src/ipc/protocol.ts`, add the mirrored optional type: `freshness?: { kind: "fresh" } | { kind: "stale"; revalidating: boolean } | { kind: "unverified"; reason: string }` on the `ImportResult` interface. No consumer reads it yet.
- [ ] **Step 5:** Run the round-trip test + `cargo test` green; `pnpm --filter extension typecheck` green; clippy.
- [ ] **Step 6:** Commit `feat(daemon): add ResultFreshness data-layer flag`.

---

## Task 3: D3 — first-party deps bypass the TTL fast-path

**Files:**
- Modify: `daemon/src/cache/key.rs` (`cache_key_is_first_party`)
- Modify: `daemon/src/cache/memory.rs` (`get` fast-path gate)
- Test: `daemon/tests/freshness_core.rs`

**Interfaces:**
- Consumes: `decode_cache_identity`, `CacheIdentity.entry_path`.
- Produces: `pub fn cache_key_is_first_party(key: &str) -> bool` — true when the decoded `entry_path` contains no `node_modules` path component (workspace / `npm link` / `file:` dep). Derived from the key; no `CachedImport` schema change.

- [ ] **Step 1: Failing test** — build a key for a resolved dep whose entry is under `node_modules/` → `cache_key_is_first_party` is false; build one whose entry is a sibling workspace path (no `node_modules`) → true. Then assert that an inserted first-party entry, within the same generation + TTL, is **re-verified** on `get` (does NOT take the fast path): delete/modify its dep and assert `get` reflects the change immediately (vs a node_modules dep which serves on the fast path within TTL).
- [ ] **Step 2:** Run — FAIL (fn missing; fast path taken for all).
- [ ] **Step 3:** Add `cache_key_is_first_party`: `decode_cache_identity(key).and_then(|id| id.entry_path).map_or(false, |p| !p.split('/').any(|seg| seg == "node_modules"))`. (The identity path is normalized `/`-separated — confirm against `normalize_identity_path`.)
- [ ] **Step 4:** In `memory.rs` `get`, gate the fast path: `let fresh_without_restat = !cache_key_is_first_party(key) && cached.verified_generation == generation && cached.verified_at.is_some_and(|at| at.elapsed() < REVERIFY_TTL);`. First-party keys always fall to the slow-path `check_fingerprints`.
- [ ] **Step 5:** Tests green; clippy. Note the per-get `decode_cache_identity` cost is acceptable (only on the fast-path check; measure if a large-workspace concern surfaces).
- [ ] **Step 6:** Commit `fix(daemon): re-verify first-party deps every get (D3)`.

---

## Task 4: Serve-stale + in-flight dedupe (daemon serve side, no delivery yet)

**Files:**
- Modify: `daemon/src/cache/memory.rs` (freshness-aware get + in-flight set)
- Modify: `daemon/src/service.rs` (`analyze_with_cache` propagates freshness + a "needs revalidation" signal)
- Test: `daemon/tests/freshness_core.rs`

**Interfaces:**
- Produces: `ImportCache::get_with_result_freshness(&self, key: &str) -> Option<(ImportResult, ResultFreshness)>` (non-evicting for `Stale`); an in-flight `Mutex<HashSet<String>>` (mirroring the registry single-flight) with `begin_revalidation(key) -> bool` (true = this caller owns the revalidation) and `finish_revalidation(key)`.
- **Behavior change:** the `Stale`→`remove + None` arm (memory.rs ~127) becomes: clone the last value, stamp `freshness = Stale { revalidating: true }`, return `Some`, and mark the key in-flight. `Gone` still evicts (drop + `None`, never served). `Unknown` serves the last value stamped `Unverified { reason }` (from the stat error). `Fresh` serves `Fresh` as today.

- [ ] **Step 1: Failing test** — insert an entry, change a dep so it's `Stale` (still exists), bump generation; `get_with_result_freshness` returns `Some((old_value, Stale{revalidating:true}))` and the entry is NOT removed (a second `get` still returns it, still flagged Stale, and `begin_revalidation` returns true once then false for the dupe). A `Gone` dep (deleted) still returns `None`.
- [ ] **Step 2:** Run — FAIL.
- [ ] **Step 3:** Add the in-flight `Mutex<HashSet<String>>` field + `begin/finish_revalidation` (use a drop-guard like the registry's `InflightFetchGuard` so a panicking revalidation still clears the key). Implement `get_with_result_freshness` reusing the tri-state match but mapping `Stale→serve-stale + mark in-flight`, `Unknown→Unverified`, `Gone→None`, `Fresh→Fresh`.
- [ ] **Step 4:** In `service.rs` `analyze_resolved_with_cache`, call `get_with_result_freshness`; on `Some((result, freshness))` where `freshness` is `Stale{revalidating:true}` AND `begin_revalidation(key)` was true, record that a revalidation is needed for `(workspace, key, request)` (return it up so Task 5 can spawn the push). Return the served (stale-flagged) result immediately.
- [ ] **Step 5:** Tests green; clippy.
- [ ] **Step 6:** Commit `feat(daemon): serve stale sizes with in-flight dedupe`.

---

## Task 5: Background revalidation + push delivery (the load-bearing part)

**Files:**
- Modify: `daemon/src/ipc/protocol.rs` (`RefreshedResults` message)
- Modify: `daemon/src/ipc/server.rs` (`ServerOutboundMessage::RefreshedResults` + drainer + spawn on stale serve)
- Modify: `daemon/src/service.rs` (a `revalidate_and_report(key, request, workspace, sink)` that recomputes via `analyze_and_cache` and emits the fresh result)
- Test: `daemon/tests/server.rs` (mirror `server_streams_registry_hint_partials_before_final_response`)

**Interfaces:**
- Produces: `ServerOutboundMessage::RefreshedResults(RefreshedResultsMessage { workspace_root, document_path, results: Vec<ImportResult> })` — a *second* server→client push keyed so the extension can locate the entries (results carry their `specifier`; document+workspace locate the store row). Revalidation runs on the existing background lane (`spawn_blocking` or the rayon executor used for registry refresh) and delivers via the existing `outbound_tx` (`ServerOutboundMessage`) — never the (closed) request response.

- [ ] **Step 1: Failing test** — drive an analyze/file-size request whose cache entry is stale; assert (a) the immediate response carries the stale value flagged `Stale{revalidating:true}`, then (b) a later unsolicited `RefreshedResults` frame arrives carrying the recomputed value flagged `Fresh` for the same document+specifier. Use the `tokio::io::duplex` + framed-read harness from `server.rs`.
- [ ] **Step 2:** Run — FAIL (no variant / no push).
- [ ] **Step 3:** Add the `RefreshedResults` variant to `ServerOutboundMessage` and a matching protocol message; add a drainer arm in `handle_connection`'s `outbound_rx` select (mirror `RefreshRegistryHints` at server.rs ~526-593).
- [ ] **Step 4:** When Task 4 signals "revalidation needed", spawn it (clone the `outbound_tx`): recompute the single import via `analyze_and_cache` (which re-inserts the fresh entry + stamps a fresh generation), then `outbound_tx.send(ServerOutboundMessage::RefreshedResults(...))` with the fresh result (`freshness = Fresh`), and call `finish_revalidation(key)`. Guard delivery so a client that disconnected doesn't panic (the send just errors — log at debug).
- [ ] **Step 5:** Test green (the two-frame assertion); `cargo test` green; clippy. Verify no revalidation storm: repeated stale gets for the same key spawn ONE revalidation (the in-flight set from Task 4).
- [ ] **Step 6:** Commit `feat(daemon): push refreshed sizes after background revalidation`.

---

## Task 6: Extension client — apply pushed refreshes to the store

**Files:**
- Modify: `extension/src/ipc/protocol.ts` (the `RefreshedResults` inbound message)
- Modify: `extension/src/ipc/client.ts` (route the unsolicited message to a handler, not a pending promise)
- Modify: `extension/src/analysis/state.ts` (a method to merge refreshed results into `AnalysisStore` and fire `onDidChange`)
- Test: extension unit test if the harness supports it; otherwise a typecheck + a manual verification note

**Interfaces:**
- Consumes: `RefreshedResultsMessage`.
- Produces: `AnalysisStore.applyRefreshedResults(documentUri, results)` — updates the matching `ImportAnalysisState.result` rows by specifier and fires `onDidChange` so decorations re-render with the fresh value/flag.

- [ ] **Step 1:** Add the inbound message type in `protocol.ts`. In `client.ts`, the read loop currently resolves pending promises by `request_id`. **Do NOT reuse the registry-hint `onPartial` path** — that path looks up a *live pending entry* by `request_id` and returns early if none exists (`client.ts:365`), but a `RefreshedResults` push is **unsolicited**: the original request already resolved and its pending entry is gone. Instead, dispatch by **message type** to a standalone handler (no `request_id` lookup) that routes to `AnalysisStore.applyRefreshedResults`, keying by `document_path` + `workspace_root` carried in the message.
- [ ] **Step 2:** Wire the callback to `AnalysisStore.applyRefreshedResults`, which locates the document's states by URI, replaces the `result` for each matching specifier (preserving position/order), and fires `onDidChange`.
- [ ] **Step 3:** `pnpm --filter extension typecheck` + Biome green. Add/adjust an extension test if one exists for the client message routing; otherwise document the manual check (open a file, edit a workspace dep, confirm the size updates in place without a flash).
- [ ] **Step 4:** Commit `feat(extension): apply pushed refreshed sizes to the store`.

---

## Task 7: CI / CLI force-fresh bypass

**Files:**
- Modify: `daemon/src/ipc/protocol.rs` (a `#[serde(default)] force_fresh: bool` on the analyze + file-size requests)
- Modify: `daemon/src/service.rs` (thread `force_fresh` into `analyze_with_cache` → skip SWR serve-stale; compute synchronously)
- Modify: `cli/importlens.mjs` (set `force_fresh: true` on its `file_size_document` request)
- Test: `daemon/tests/service.rs`

**Interfaces:**
- Produces: a request-level `force_fresh: bool` (default false). When true, `analyze_with_cache` bypasses `get_with_result_freshness`'s serve-stale (and any disk-hydrated stale) and recomputes synchronously, returning a `Fresh` result; it may still write-through to the cache.

- [ ] **Step 1: Failing test** — a stale cache entry + a `force_fresh: true` request returns the *recomputed* (fresh) value synchronously, never the stale one; a `force_fresh: false` request returns the stale value flagged `Stale`.
- [ ] **Step 2:** Run — FAIL.
- [ ] **Step 3:** Add the `force_fresh` field (default false) to the analyze/file-size request structs + the extension protocol mirror (optional, default false). In `analyze_with_cache`, when `force_fresh`, skip the serve-stale path and call `analyze_and_cache` directly (or a "recompute even if a stale hit exists" path).
- [ ] **Step 4:** In `cli/importlens.mjs`, add `force_fresh: true` to the `file_size_document` request payload.
- [ ] **Step 5:** Tests green; `cargo test`; clippy; `pnpm --filter extension typecheck`.
- [ ] **Step 6:** Commit `feat(daemon): force-fresh request flag for CI`.

---

## Self-Review (against spec §4.3/§4.4/§4.5/§4.3.1)

- §4.5 SWR serve-stale + in-flight dedupe → Tasks 4–5. Data-layer flag → Task 2. ✅
- §4.3.1 `Unknown`→`Unverified` graduation → Task 4 (serve last value flagged `Unverified{reason}`); *note:* the spec's transient-retry-then-persistent graduation (retry a few times before surfacing) is simplified here to "serve `Unverified` immediately on a non-`NotFound` error" — flag for the reviewer whether the retry-window is in-scope for Plan 3 or a follow-up.
- §4.4 D3 first-party bypass → Task 3. ✅
- §4.5 CI forces fresh → Task 7. ✅
- Freshness-hardening prerequisite (integrity-review Minor) → Task 1. ✅
- **Delivery risk called out:** Task 5 is the novel work (no existing second-push path); its test mirrors the proven `server_streams_registry_hint_partials_before_final_response` harness.
- **Placeholder note:** the exact `file:line` anchors (memory.rs get arms, server.rs outbound drainer, the `ImportResult {` literal sites, client.ts read loop) MUST be re-confirmed by a planning-brief + verification pass at execution time — the daemon serve path is intricate and this plan was authored from a survey, not a line-locked diff.
- **Ambiguity to resolve at execution:** whether revalidation runs on `spawn_blocking` vs the rayon registry executor (both viable; pick per the load characteristics — bundle recompute is CPU-bound, so the rayon lane matches).
