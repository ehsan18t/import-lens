# Retry-After HTTP-Date Parsing And SRS Tree Refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore HTTP-date `Retry-After` parsing that the daemon migration dropped (the old TS registry client handled both delta-seconds and HTTP-date), and bring the SRS file tree up to date with the daemon `registry/`/`report/` modules and the extension's `registryRefresh.ts`.

**Architecture:** The `Retry-After` fix is confined to one private function in the daemon's registry HTTP client: parse numeric delta-seconds first (exact current semantics), then fall back to RFC 7231 HTTP-date via the `httpdate` crate, computing the delta against an injected `SystemTime` so the function stays unit-testable. On any parse failure the function still returns `None`, and the existing caller fallback (`transient_backoff_ms`) is unchanged. The SRS fix is a documentation-only tree edit.

**Tech Stack:** Rust 2024 (daemon crate `import-lens-daemon`, lib `import_lens_daemon`), `httpdate ^1` (new tiny zero-dependency crate), inline `#[cfg(test)]` unit tests (codebase convention: see `daemon/src/registry/service.rs`, `daemon/src/report/model.rs`).

## Global Constraints

- Branch: `feature/daemon-boundary-migration`; commit on top of `3110850`.
- Do not change the wire protocol, `HttpRegistryResponse`, or `RegistryHttpClient` — the fix is internal to `client.rs`.
- Numeric `Retry-After` behavior must remain byte-identical: `parse::<f64>()`, negative clamped to 0, `seconds * 1000` rounded.
- Old-TS parity reference (deleted on this branch; readable via `git show main:extension/src/guidance/registryHints.ts`, lines 299–318): numeric first, then `Date.parse`, past dates clamp to 0, unparseable → null.
- Run `cargo fmt` before each Rust commit. Never touch `README.md`.

---

### Task 1: HTTP-Date Retry-After Parsing In The Registry Client

**Files:**
- Modify: `daemon/Cargo.toml` (add `httpdate`)
- Modify: `daemon/src/registry/client.rs` (function at lines 66–71, call site at line 44, imports at line 2)
- Test: inline `#[cfg(test)] mod tests` appended to `daemon/src/registry/client.rs`

**Interfaces:**
- Consumes: nothing new from other tasks.
- Produces: `fn retry_after_delay_ms(header: &str, now: SystemTime) -> Option<u64>` (private; call site inside `get_package_metadata` updated to pass `SystemTime::now()`). No public API change.

- [ ] **Step 1: Add the `httpdate` dependency**

In `daemon/Cargo.toml` `[dependencies]`, insert alphabetically (between `futures-util` and `oxc_allocator`):

```toml
httpdate = "^1"
```

- [ ] **Step 2: Write the failing tests**

Append to the end of `daemon/src/registry/client.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::retry_after_delay_ms;
    use std::time::{Duration, SystemTime};

    #[test]
    fn retry_after_parses_delta_seconds() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(retry_after_delay_ms("120", now), Some(120_000));
        assert_eq!(retry_after_delay_ms("1.5", now), Some(1_500));
    }

    #[test]
    fn retry_after_clamps_negative_delta_seconds_to_zero() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(retry_after_delay_ms("-5", now), Some(0));
    }

    #[test]
    fn retry_after_parses_future_http_date() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let header = httpdate::fmt_http_date(now + Duration::from_secs(30));
        assert_eq!(retry_after_delay_ms(&header, now), Some(30_000));
    }

    #[test]
    fn retry_after_clamps_past_http_date_to_zero() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let header = httpdate::fmt_http_date(now - Duration::from_secs(30));
        assert_eq!(retry_after_delay_ms(&header, now), Some(0));
    }

    #[test]
    fn retry_after_rejects_unparseable_values() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(retry_after_delay_ms("soon", now), None);
        assert_eq!(retry_after_delay_ms("", now), None);
    }
}
```

Note: this will not compile yet (`retry_after_delay_ms` takes one argument) — that is the failing state for a signature change.

- [ ] **Step 3: Run tests to verify failure**

Run: `cargo test -p import-lens-daemon --lib registry::client`
Expected: FAIL — compile error `this function takes 1 argument but 2 arguments were supplied` (E0061).

- [ ] **Step 4: Implement the parser**

In `daemon/src/registry/client.rs`, change line 2 from:

```rust
use std::time::Duration;
```

to:

```rust
use std::time::{Duration, SystemTime};
```

Replace the function at lines 66–71:

```rust
fn retry_after_delay_ms(header: &str) -> Option<u64> {
    header
        .parse::<f64>()
        .ok()
        .map(|seconds| (seconds.max(0.0) * 1000.0).round() as u64)
}
```

with:

```rust
fn retry_after_delay_ms(header: &str, now: SystemTime) -> Option<u64> {
    if let Ok(seconds) = header.parse::<f64>() {
        return Some((seconds.max(0.0) * 1000.0).round() as u64);
    }

    // RFC 7231 allows Retry-After to carry an HTTP-date instead of
    // delta-seconds; proxies/CDNs in front of registries emit this form.
    // A past date clamps to zero (retry immediately), matching the old
    // extension-host parser this daemon client replaced.
    let retry_at = httpdate::parse_http_date(header).ok()?;
    Some(
        retry_at
            .duration_since(now)
            .map(|delay| delay.as_millis() as u64)
            .unwrap_or(0),
    )
}
```

Update the call site at line 44 from:

```rust
            .and_then(retry_after_delay_ms);
```

to:

```rust
            .and_then(|value| retry_after_delay_ms(value, SystemTime::now()));
```

- [ ] **Step 5: Run tests to verify pass**

Run: `cargo test -p import-lens-daemon --lib registry::client`
Expected: PASS — 5 tests, 0 failed.

- [ ] **Step 6: Run the full daemon suite and format**

Run: `cargo test -p import-lens-daemon` then `cargo fmt` then `cargo fmt --check`
Expected: all suites green (no behavior change for numeric headers, so `daemon/tests/registry.rs` stays green); fmt clean.

- [ ] **Step 7: Commit**

```powershell
git add daemon/Cargo.toml daemon/src/registry/client.rs Cargo.lock
git commit -m "fix: parse http-date retry-after headers in registry client"
```

---

### Task 2: Refresh The SRS File Tree

**Files:**
- Modify: `docs/ImportLens-SRS.md` (extension guidance block at lines 1623–1626; daemon tree between lines 1704 and 1705)

**Interfaces:** none (documentation only).

- [ ] **Step 1: Add `registryRefresh.ts` to the guidance block and fix its terminator**

The block currently reads (lines 1623–1626 — note all three children use `├──`; the `└──` terminator was lost when `registryHints.ts` was removed):

```text
│   │   ├── guidance/
│   │   │   ├── packageJsonAnalysis.ts # daemon-backed package.json dependency analysis controller
│   │   │   ├── packageJsonPartial.ts  # indexed package.json partial merge helpers
│   │   │   ├── packageJsonState.ts    # package.json dependency analysis state types
```

Change it to:

```text
│   │   ├── guidance/
│   │   │   ├── packageJsonAnalysis.ts # daemon-backed package.json dependency analysis controller
│   │   │   ├── packageJsonPartial.ts  # indexed package.json partial merge helpers
│   │   │   ├── packageJsonState.ts    # package.json dependency analysis state types
│   │   │   └── registryRefresh.ts     # daemon registry refresh orchestration and stale-hint state
```

- [ ] **Step 2: Add the daemon `registry/` and `report/` modules to the tree**

Between the `cache/` block (ends line 1704, `│       │   └── project.rs ...`) and the `lifecycle.rs` line (1705), insert:

```text
│       ├── registry/
│       │   ├── mod.rs
│       │   ├── constants.rs           # registry TTL, timeout, retry, and concurrency constants
│       │   ├── types.rs               # normalized npm package metadata and cache entry types
│       │   ├── client.rs              # bounded ureq npm registry HTTP client
│       │   ├── cache.rs               # persistent JSON package metadata cache (atomic writes)
│       │   ├── service.rs             # refresh modes, single-flight de-dup, retry, stale fallback
│       │   └── executor.rs            # dedicated registry refresh worker pool
│       ├── report/
│       │   ├── mod.rs
│       │   ├── executor.rs            # bounded workspace report worker pool
│       │   ├── scanner.rs             # symlink-safe workspace source scanner
│       │   └── model.rs               # report rows, summary counts, duplicate groups, treemap
```

- [ ] **Step 3: Verify the tree edits**

Run:

```powershell
rg -n "registryRefresh\.ts|├── registry/|├── report/" docs/ImportLens-SRS.md
```

Expected: exactly 3 matches (one per addition). Then visually confirm the guidance block ends with `└──` and the box-drawing column alignment matches neighboring blocks (`document/`, `cache/`).

- [ ] **Step 4: Commit**

```powershell
git add docs/ImportLens-SRS.md
git commit -m "docs: refresh SRS file tree for daemon registry and report modules"
```

---

## Verification (whole plan)

1. `cargo test -p import-lens-daemon` — all green, including the 5 new client tests.
2. `cargo fmt --check` — clean.
3. `rg -n "retry_after_delay_ms" daemon/src` — exactly the definition and the single call site passing `SystemTime::now()`.
4. `git status --short` — clean tree after both commits.
5. Parity spot-check: `git show main:extension/src/guidance/registryHints.ts | sed -n '299,318p'` — confirm the Rust semantics (numeric → date → None, past-date clamp) mirror the old TS.

## Self-Review

- **Spec coverage:** Finding 1 (HTTP-date parsing + unit test) → Task 1 Steps 1–7. Finding 2 (SRS tree: missing daemon modules) → Task 2 Step 2; the sub-findings my verification added (missing `registryRefresh.ts`, lost `└──` terminator) → Task 2 Step 1. The stale-deleted-files half of Finding 2 needs no task — already fixed at `3110850`.
- **Placeholder scan:** none — every step carries exact code, paths, commands, and expected output.
- **Type consistency:** `retry_after_delay_ms(header: &str, now: SystemTime) -> Option<u64>` is used identically in Steps 2, 4, and the call site; test values (30s → 30_000 ms) are consistent with millisecond returns.
- **Sizing note:** `httpdate::fmt_http_date` truncates to whole seconds, so constructing the expected header from `now + 30s` (a whole-second timestamp) keeps `Some(30_000)` exact — no tolerance window needed.
