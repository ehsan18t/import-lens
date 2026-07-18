# The Import Lens Bundler

The single source of truth for how Import Lens turns an import statement into a number.

---

## 1. The question the bundler exists to answer

*How many bytes does this import actually cost?*

Not "how big is this package" — that question is easy and useless. The cost of importing one
function from a library is the size of the subgraph that function drags in: its own code, plus
every module it transitively reaches, minus everything the bundler can prove is unused, after
minification and compression.

Answering that honestly requires linking a module graph and tree-shaking it. There is no
shortcut. Summing file sizes over-counts wildly; reading the package manifest tells you
nothing; parsing the entry file alone tells you almost nothing. **The only truthful answer is
the one a real bundler produces**, which is why Import Lens contains one.

Everything else in this document follows from that single commitment.

---

## 2. The core decision: we do not implement bundling

Import Lens embeds **Rolldown** — a production Rust bundler built on the OXC compiler toolchain
— and gives it total authority over JavaScript semantics. Import Lens keeps only the work a
bundler does not do.

| Rolldown owns | Import Lens owns |
| --- | --- |
| Resolving and loading the transitive graph | Deciding *which package* the user meant |
| Linking imports, exports, re-exports, namespaces | Deciding *what to measure* |
| Interpreting `sideEffects` and deciding retention | Measuring, compressing, and reporting |
| Deconflicting symbols, emitting the linked chunk | Caching, freshness, and staying fast |
| Every question of "is this code reachable?" | Every question of "have I answered this before?" |

**Why this line, and not a little further to either side.**

Import Lens used to implement bundling itself, and it was a slow-motion disaster. The old
engine made three separate decisions — which exports are reachable, which modules to include,
which statements to emit — and nothing forced them to agree. When they disagreed, its fallbacks
converted the disagreement into *believable wrong numbers*: one path kept every import when it
couldn't tell which mattered (over-counting), another invented the name of a binding it
couldn't resolve (emitting a reference to a symbol no module declared, under-counting). One
real package under-reported by a third. The test suite passed the entire time, because it
tested individual rewriting cases rather than whether the emitted bundle was closed.

The lesson is sharp enough to be a rule: **a tool whose only output is a number may not contain
a code path that guesses.** Bundler semantics are not a feature Import Lens can own part of. It
either owns all of them and gets them right, or owns none of them. It owns none of them.

The corollary is that Import Lens must never quietly grow a second bundler. The moment
something in the product starts asking "is this statement live?" or "does this glob match?" or
"what does this re-export chain resolve to?", the old failure is back.

---

## 3. When the bundler runs

The bundler is expensive, so it runs only when something the user did makes a number
potentially wrong. Six things do that.

```mermaid
graph LR
    subgraph editor["What the user does"]
        A["Opens / edits<br/>a JS · TS · Vue · Astro file"]
        B["Opens<br/>package.json"]
        C["Types inside<br/>import { … }"]
        D["Asks for a<br/>workspace report"]
        E["Connects<br/>(daemon starts)"]
        F["Installs / removes<br/>packages"]
    end

    A --> A1["Size every import in the file<br/><i>+ the file's combined total</i>"]
    B --> B1["Size every declared dependency"]
    C --> C1["List the package's exports"]
    D --> D1["Size every import in every file"]
    E --> E1["Prewarm: rebuild recently-used<br/>entries before they're asked for"]
    F --> F1["Invalidate — nothing rebuilds<br/>until something is asked for"]

    style A1 fill:#1a365d,color:#fff
    style B1 fill:#1a365d,color:#fff
    style C1 fill:#1a365d,color:#fff
    style D1 fill:#1a365d,color:#fff
    style E1 fill:#744210,color:#fff
    style F1 fill:#742a2a,color:#fff
```

### Opening or editing a source file

The most common trigger. The document is parsed, every package import is extracted, and each
one is sized. This is what produces the annotation next to each import statement.

Two distinct measurements come out of it:

- **Per-import sizes** — one number per import statement, each measured independently, as if
  that import were the only thing in the file.
- **The file's combined total** — a *single* measurement of all the file's imports built
  together, so shared dependencies are counted once. Two imports that both pull in the same
  helper library cost less together than the sum of their individual numbers, and the combined
  total is the only figure that reflects that.

Editing is debounced in the editor, so a burst of keystrokes produces one analysis.

**The answer does not wait for the builds.** The response comes back immediately with every
import the cache could answer, and each import still being built is marked as *Loading* and
**pushed to the editor as it lands**. This is not a performance nicety — it is what makes one
slow import survivable. The response used to be all-or-nothing, so a single pathological package
could push it past the editor's deadline and the editor would then discard **the entire
document's results, including the nineteen imports already answered from cache**. Now a
pathological package delays *its own number* and nothing else. (§9 explains why a package can be
pathological in the first place.)

### Opening `package.json`

Every declared dependency is sized as if it were imported for its whole surface. This answers
"what am I paying for by depending on this at all", which is a different question from "what am
I paying for by importing this one function" — and it is deliberately the more pessimistic one.

### Typing inside an import's braces

Completions need the list of names a package exports. This is *not* a size measurement, but it
is still an engine build: the only trustworthy list of exports is the one the bundler resolves,
after following every re-export chain and star export. Guessing it — by walking export
statements with a hand-written parser — is exactly the class of thing that broke the old
engine.

The same list powers the "convert namespace import to named imports" action.

### The workspace report

A bulk sweep: every file, every import. It runs on a dedicated worker pool so it cannot starve
interactive work, and its cache reads are marked as bulk so a report cannot flood the recency
signal and evict the entries the user is actively working with.

### Daemon startup

Nothing the user did, but worth knowing: on connect, the daemon replays the most recently used
cache entries and rebuilds any that have gone stale, so the first file the user opens is
already warm. Any real user request cancels this instantly — prewarm never makes a user wait.

### Package installs

An install does not trigger a build. It **invalidates**, and the next request pays for the
rebuild. Rebuilding an entire dependency tree on `npm install` would be enormous, mostly
wasted, and would fight the very install that triggered it.

---

## 4. How each kind of import is handled

Every import the bundler measures is expressed as one of four *selections*. The selection
decides what the bundler is asked to keep alive, and therefore what gets measured.

| What the user wrote | Selection | What is measured |
| --- | --- | --- |
| `import { format } from "date-fns"` | **Named** | Only the subgraph `format` reaches. Everything else in the package is tree-shaken away. |
| `import React from "react"` | **Default** | The default export and its subgraph. |
| `import * as _ from "lodash"` | **Namespace** | The entire public surface — a namespace object references everything, so nothing can be shaken. |
| `await import("chart.js")` | **Full** | The entire package. A dynamic import loads the whole module, so its cost is the whole module. |
| `import "some-polyfill"` | **Namespace** | A bare import has no bindings; it is measured for its whole surface, because its purpose is its side effects. |
| `export { x } from "pkg"` | **Named** | A re-export is an import that also republishes. Measured as the named import it is. |

Some further nuances that are easy to get wrong:

- **One statement can produce two measurements.** `import React, { useState } from "react"`
  is a default import *and* a named import. They are separate rows with separate numbers,
  because they answer separate questions.
- **A named import with no names** — which can happen through some syntax paths — falls back to
  Full. When the bundler cannot tell what surface is wanted, it measures the pessimistic one.
- **Type-only imports cost nothing and are elided.** A TypeScript import whose every reference
  is in type position disappears at compile time, so it is removed before measurement rather
  than reported as zero. An import with *no* references at all is deliberately **not** elided —
  it might be there for its side effects.
- **Dynamic imports do not create a second chunk.** Code splitting is disabled, so a
  dynamically-imported dependency inlines into the single measured chunk. This is the correct
  model: the user is asking what that lazy chunk *costs*, not how the bundler would split it.

### How the bundler is told what to keep

The bundler is never handed the user's bare specifier. Import Lens has already resolved
`date-fns` to an exact file on disk — it had to, in order to know what to cache — and asking
the bundler to resolve it a second time invites two resolutions to disagree, or to pick a
different copy of the package in a nested workspace.

Instead, Import Lens synthesizes a tiny **virtual entry module** that re-exports exactly the
requested surface from the already-resolved file, and hands *that* to the bundler as the entry
point. The bundler resolves everything transitively from there, and the requested surface is
pinned alive so tree-shaking cannot remove the very thing being measured.

This is also the mechanism behind the combined file total: the virtual entry names *all* of the
document's imports at once, so the bundler sees one graph and deduplicates shared modules
naturally. Building each import separately and adding up the results would count every shared
dependency once per importer — which is the wrong number, and the reason the bundler is never
asked to build the same document's imports as independent bundles.

---

## 5. What actually happens during a build

```mermaid
graph TD
    START([an import needs a number]) --> RES[resolve the package<br/>to an exact entry file]
    RES --> VE[synthesize the virtual entry<br/>naming the requested surface]
    VE --> BUILD

    subgraph BUILD["the bundler"]
        L1[resolve + load the graph] --> L2[parse · transform<br/>TS · JSX · JSON]
        L2 --> L3[link bindings, namespaces,<br/>re-exports, CJS interop]
        L3 --> L4[tree-shake:<br/>drop everything unreachable]
        L4 --> L5[deconflict + emit<br/>ONE linked chunk]
    end

    BUILD --> RAW[raw bytes]
    BUILD --> MODS[per-module contributions]
    BUILD --> PATHS[every file touched<br/>— including shaken ones]
    RAW --> MIN[minify]
    MIN --> COMP[gzip · brotli · zstd]
    COMP --> DONE([the numbers])
    PATHS --> FRESH([freshness record])

    style BUILD fill:#1a365d,color:#fff
    style DONE fill:#22543d,color:#fff
```

Three things about this flow are non-obvious and load-bearing.

**The build produces exactly one chunk.** Not "usually one" — exactly one, with no extra
assets. Any other output shape is treated as a failure, because a second chunk means something
was split off and would not be counted.

**The raw and minified numbers come from the same link pass.** The bundler can minify its own
output, but asking it to would mean linking the graph *twice* — once to get the unminified
chunk, once to get the minified one — doubling the cost and risking the two measurements
observing different states of the filesystem. Instead the bundler emits one unminified chunk,
and Import Lens minifies that chunk itself with OXC's minifier. Compression then runs over the
minified string, never the raw one.

**Every file the graph touched is recorded — including the ones tree-shaken away.** This looks
like waste and is the opposite. Editing a module that was *excluded* from the last build can
change what gets included in the next one: it can change an export, add a side effect, or break
a re-export chain. A cache entry that only tracked the modules it rendered would happily serve
a stale number forever after such an edit. In practice this gap is large — one common package
loads three hundred files to render thirty-six.

### Limits

A build is bounded, because a pathological package must not be able to exhaust the machine:
a ceiling on module count, on any single module's size, and on the total source loaded. An
oversized module is rejected *before* it is read — reading it first would blow the very bound
being enforced. A breach is a clean, typed failure, never a crash and never a partial graph.

---

## 6. The numbers, and where they come from

| Reported | What it means |
| --- | --- |
| **Raw** | The linked chunk, unminified. The honest "this is the code you're pulling in". |
| **Minified** | That chunk through a real minifier. |
| **Gzip / Brotli / Zstd** | The minified string, compressed three ways. This is what actually crosses the wire. |
| **Module breakdown** | Which modules contributed the most bytes — the top handful, so a user can see *why* an import is expensive. |
| **Shared bytes** | Of this import's cost, how much is shared with *other* imports in the same file. A dependency two imports both use is not free, but it is not paid for twice either. |
| **Truly tree-shakeable** | Whether importing one name is meaningfully cheaper than importing the whole package. |
| **Confidence** | Whether the number is fully trustworthy, or was produced with a caveat. |

**Per-module contributions are approximate, on purpose.** They are measured before the final
minification pass, and the chunk's glue code belongs to no single module — so the parts do not
sum to the whole and are not required to. They exist to answer "what is making this big", not
to be re-added. Scaling them to force them to sum would be inventing precision that does not
exist.

**"Truly tree-shakeable" costs a second build.** To know whether pulling one export is cheaper
than pulling everything, you have to know what everything costs — so the package is built a
second time at its full surface, and the two are compared. This is expensive, so it is skipped
where it cannot be meaningful (a package that declares side effects, or an import that already
takes the whole surface), and the full-package result is remembered per package rather than per
import — otherwise ten different named imports from one library would each pay for their own
identical full build.

**Confidence is a first-class output, not a footnote.** When the bundler emits a warning, when a
package's side-effect declaration prevents a clean answer, or when a package ships bytes we did
not count, the user is told. A number that quietly hides its own uncertainty is worse than no
number.

But confidence is a *qualifier on a real measurement* — it is not a licence to report a made-up
one. There is no such thing as a low-confidence guess here: a build either produced a number or
it did not (§9). The product once used confidence that way, and a badge is no defence against a
byte count that is wrong by an order of magnitude, because users read the number.

---

## 7. How rebuilding is avoided

A build is the most expensive thing the product does. Almost every request could have been
answered without one. The entire performance story is layers of "have I already answered this?"

```mermaid
graph TD
    REQ([a request arrives]) --> Q1{Has this exact file's<br/>import set been sized<br/>recently and unchanged?}
    Q1 -->|yes| SERVE([answer, no build])
    Q1 -->|no| Q2{Is this import in memory,<br/>and are the files it was<br/>measured from unchanged?}
    Q2 -->|yes| SERVE
    Q2 -->|no| Q3{Is it on disk from<br/>a previous session,<br/>and still fresh?}
    Q3 -->|yes| HYD[pull into memory] --> SERVE
    Q3 -->|no| Q4{Is an identical build<br/>already running<br/>right now?}
    Q4 -->|yes| JOIN[wait for it,<br/>share its result] --> SERVE
    Q4 -->|no| BUILD[build]
    BUILD --> STORE[record the result and<br/>every file it depended on] --> SERVE

    style BUILD fill:#742a2a,color:#fff
    style SERVE fill:#22543d,color:#fff
```

Each layer answers a different kind of repetition:

- **The document layer** catches the same file being re-analyzed while nothing in it changed —
  the overwhelmingly common case, since the editor asks again on every pause in typing.
- **The memory layer** catches the same import appearing in a different file, or the same file
  after an unrelated edit.
- **The disk layer** catches the same import across daemon restarts and across sessions. It is
  what makes reopening a project fast rather than a stampede.
- **Single-flight** catches the same import being requested twice *simultaneously* — a file with
  the same package imported in two statements, or two files opened at once. Only one build runs;
  the rest wait for it.
- **The side-build memos** catch the repeated work *inside* a build path: the full-package
  comparison and the export list are per-package facts, not per-import ones, so they are
  remembered per package.

And the things that avoid a build without any cache at all:

- **Cache hits never queue behind builds.** Hits are resolved on the full-width worker pool, in
  parallel, and never take a build slot. A file where nine of ten imports are cached does not
  wait on the tenth.
- **Prewarm runs before the user asks**, and is abandoned the instant they do.
- **Stale-while-revalidate** serves a known-stale number immediately, then quietly pushes the
  corrected one when the rebuild lands. A user staring at a file gets an instant answer that may
  be slightly out of date, rather than a spinner that is precisely correct.

### Concurrency: two bounds that are easy to confuse

Two builds may run at once. That number bounds **peak memory** — a build holds an entire module
graph in RAM, and the daemon shares a machine with the editor.

Each running build, however, may use most of the machine's cores, because the bundler
parallelizes *within* a build. These are different bounds and conflating them was a real bug:
the build's thread pool was once sized to the build *concurrency* limit, which pinned every
build to two threads no matter how many cores existed. Separating them made real-package builds
substantially faster without changing memory usage at all.

### A build can hang forever, and that is not our bug to fix

The bundler fans each module out onto its own async task, and **the async runtime swallows a
panic in a spawned task**. When one of those module tasks dies, the bundler's loader goes on
waiting for a completion message that will never arrive — on a channel it holds open itself, so
it never closes either. **The build parks. Forever.**

Catching panics at our own boundary does not see this, because nothing ever unwinds to us. So
every build carries a **hard time limit**, and that limit exists for exactly one reason: a
parked build must not hold one of the two build slots for the life of the daemon. Two of them
would wedge it permanently — no further import could ever be measured, and only a restart would
recover it.

This is the *only* timeout the design has. Nothing else needs one, because — since imports
stream (§3) — **no request waits on a build**. A parked build costs its own number and nothing
else. Earlier designs tried to bound the *request* instead, with a per-request deadline and with
a circuit-breaker that remembered which packages had parked. Both were deleted: neither could
bound a request that named several bad packages, and the circuit-breaker durably condemned
*healthy* packages that had merely been slow once. Bounding the build, and refusing to make
anyone wait for it, is the whole answer.

---

## 8. Freshness: knowing when an answer went wrong

A cached number is only valuable if it is *provably* still correct. Every cached result carries
the identity of every file it was measured from, and a request re-verifies them before serving.

**The identity of a cached answer** includes the package, its version, the exact resolved entry
file, the runtime it was resolved for, what kind of import it was, and — for a named import —
which names were requested. It also includes an **analyzer revision**: a marker that is bumped
whenever a change to Import Lens could move a reported number. Every stored entry records the
revision it was computed under and is rejected if that revision no longer matches. It is the
one thing standing between a user and a number produced by code that no longer exists. Changing
how a size is measured without bumping it is the most dangerous mistake available in this
codebase.

**Verification is split by who can change the file.**

- **Installed dependencies** are verified cheaply — size and modification time. Files inside a
  package do not change without an install, and an install invalidates everything anyway. Paying
  to re-read and hash hundreds of dependency files on every keystroke would be absurd.
- **First-party files** are verified strictly: re-read and re-hashed, every time. They change
  constantly, and a modification timestamp is not trustworthy for a file the user is actively
  editing.

**The bytes are fingerprinted at the moment they are read, during the build itself** — not
afterwards. This is subtle and it matters. If a file is edited *while* a build is running, and
the fingerprint were taken afterwards by re-reading from disk, the cache would store the NEW
file's hash against a size measured from the OLD file's contents. Every future check would then
compare the file against a hash it matches, conclude "fresh", and serve that wrong number
forever — the entry could never heal. Hashing during the read closes the window: the
fingerprint always describes exactly the bytes that were measured.

**A file that cannot be read is not the same as a file that changed.** A transient failure —
a locked file, a directory being rewritten by an installer — is treated as *unknown*, not as
stale. Unknown declines to serve the cached answer, but does not throw it away, and does not
trigger a rebuild against a filesystem that is mid-flight.

---

## 9. When the build fails

It will. Packages ship broken syntax, unresolvable optional dependencies, and graphs that blow
every reasonable limit. The rule is exactly one sentence:

> **A size exists if and only if a build succeeded.**

There is no fallback number, no estimate, no approximation. Every result is in one of three
states, and the state is legible from its *type* — not from a convention a reader has to know.

```mermaid
graph TD
    B[build] --> R{outcome}
    R -->|success| OK([Measured — the number])
    R -->|"still running"| LOAD([Loading — no size YET;<br/>delivered when it lands])
    R -->|"could not answer"| UN{why?}
    UN -->|"parse · link · missing export ·<br/>graph limit · output shape"| DET([Unmeasured — DETERMINISTIC<br/>a fact about the package's bytes])
    UN -->|"panic · timeout · engine gone"| TR([Unmeasured — TRANSIENT<br/>a fact about this moment])

    style OK fill:#22543d,color:#fff
    style LOAD fill:#1a365d,color:#fff
    style DET fill:#744210,color:#fff
    style TR fill:#742a2a,color:#fff
```

### Why the fallback was deleted

The product used to substitute a number when a build failed: the entry file's own bytes, or the
package's size on disk. It carried a low-confidence badge, and it looked responsible.

It was not. A large UI kit that breached a graph limit was reported at **the few kilobytes of
its barrel file** when the true answer was megabytes. Users read the byte count; a number wrong
by an order of magnitude while *looking* like a measurement is worse than no number, because it
is actionable and the action is wrong.

Worse, the fabricated result carried `error: null` **plus that plausible size** — so every
consumer that asked *"is this usable?"* by checking `!result.error` let it straight through.
That single missing distinction produced **the same defect seven times in seven different
places**: a healthy package condemned to static sizing for a whole cache generation; a
58-byte fabrication cached over a healthy 17,550-byte package; an incomplete total cached;
a fabrication written to the persisted cost history, destroying that import's real baseline;
a fabricated import *count*; and — worst — the CI gate deciding pass/fail from a fabricated
size and **silently passing**, so the regression merged.

It was never seven bugs. It was one missing model, replicated everywhere anyone needed to ask
the question. The fix is not a seventh patch: it is to make the state **unrepresentable**. With
no size to misuse, every one of those checks becomes correct by construction.

### Deterministic and request-local are not the same outcome

This is the distinction the code never made, and it is the one everything else rests on.

A **deterministic** failure — a parse error, an unresolvable link, a breached limit — is a fact
about the package's **bytes**. Same input, same outcome, forever. It **may be cached**: the
cache is already keyed by those bytes' fingerprints, so it expires exactly when the answer would
change. Refusing to cache it would re-enter the bundler for a broken package on *every* analysis,
permanently occupying one of only two build slots.

A **request-local** failure — a panic, timeout, lost engine, unreadable asset input, or failed
compressor — is a fact about **this moment's scheduling, filesystem, or machine**. It says nothing
reusable about the package. It may **never** be cached, persisted, compared against a baseline, or
turned into a pass/fail verdict. Some request-local asset outcomes carry a disclosed partial size;
the `asset_io`/`compression` diagnostic, not absence of a number, is what keeps that floor local.
Making such an outcome durable converts a momentary accident into a wrong answer that outlives the
daemon.

### Aggregates expose missing bytes structurally

A successful combined build measures its imports directly, so it stays complete while their
separate per-import analyses are still Loading. Its asset tail can still disclose supported bytes
that it could not process: `uncounted_assets` makes that otherwise-successful File Cost a structural
`incomplete` floor. If the combined build fails and the daemon instead sums per-import costs, any
Loading, Unmeasured, or measured-with-uncounted-assets contributor makes that fallback short too.
The known bytes remain worth showing, but no verdict may be drawn from them: a budget judged against
a floor is neither passed nor failed, it is *not evaluated*. `imprecise_assets` is intentionally
different: separately processed stylesheets can read high while still counting every sheet. That
deterministic upper bound remains cacheable, but it is not budgetable: its compression-boundary
inflation can produce a false failure just as a floor can produce a false pass.

And a gate that cannot measure **must never report success**. `importlens check` exits non-zero
with a code distinct from a real budget failure, so a flaky CI machine is diagnosable and is
never mistaken for a genuine regression. A silent pass is the worst outcome available — it
merges the regression.

### The one thing that is still true from before

A **missing or ambiguous export** means the user asked for a name the package does not provide.
Producing a size there would paper over a real mistake in their code. It is reported as an error
with no size — deliberately.

No failure path may invent a symbol, measure a half-linked graph, or silently substitute an
unvalidated result.

---

## 10. Trade-offs taken deliberately

The decisions most likely to look like bugs to someone who wasn't there.

**Contributions don't sum to the total.** Measured before final minification, and chunk glue
belongs to no module. Approximate by construction. (§6)

**The whole-file compressed total is a lower bound when a file mixes runtimes — and this is a
known defect, not a trade-off.** A file that imports both client and server code must be built
once per runtime, because the two resolve dependencies under genuinely different conditions.
Those results are currently compressed *together*, so an identifier appearing in both is
compressed only once.

This was originally defended on the grounds that "compressing separately and adding is no more
true, because compression is not additive." **That reasoning is wrong.** Non-additivity applies
to parts that would, in reality, be compressed *together*. Two runtime groups never are: they
are two artifacts that genuinely ship, and each is genuinely compressed on its own. Summing their
separately-compressed sizes therefore models reality **exactly** — it is the concatenation that
distorts it, by compressing away redundancy between two payloads that never meet.

**A runtime is an artifact boundary.** Compressed bytes may be summed *across* one and never
*within* one. The same rule extends to the non-JavaScript assets a package ships. Measured on a
shared-heavy two-runtime Astro file, the current concatenation under-reports by ~36%. The
correction is decided and pending.

**A namespace import is measured at full weight, with no attempt to be clever.** A namespace
object can be indexed dynamically, so nothing in the package can be proven dead. Some bundlers
optimize the easy cases; Import Lens reports the honest ceiling.

**A CommonJS package reports only a default export when its exports are listed.** Named access
to a CJS package works — it goes through interop at link time, and its size measures correctly
— but the *list* of names cannot be recovered from the bundler's output. Import Lens reports
what the bundler resolved and does not synthesize the missing names. A guessed export list is
exactly the failure that killed the old engine, and it is not worth reintroducing for a
completion popup.

**A package that declares side effects is treated pessimistically.** If a package says its
modules have side effects, tree-shaking cannot safely remove them, and the product says so
rather than quietly reporting the optimistic number.

**Installs invalidate rather than rebuild.** Cheap, correct, and it does not fight the install.

**Depending on Rolldown's Rust API is a known, contained risk.** That API carries no stability
guarantee — it can change without warning between releases. The containment is: exact version
pins on the bundler and the entire compiler stack beneath it, a recorded fingerprint of the
whole resolved dependency graph that CI checks on every change, a locked dependency file that
only an explicit upgrade command may rewrite, and a single narrow adapter that is the only
place in the codebase permitted to name a bundler type. An upgrade must re-run the full
correctness, performance, and memory qualification.

The price paid for that dependency, stated plainly: **the compiler toolchain can no longer be
upgraded independently.** The bundler pins the compiler versions it was built against, so the
whole stack moves together, on the bundler's release cadence, or not at all. This was accepted
knowingly — it is the cost of not owning bundler semantics, and it is a bargain.

---

## 11. The invariants

If you change the bundler, these are the things that must remain true. Most of them exist
because they were once false.

1. **Bundler semantics are never reimplemented.** Not reachability, not side-effect
   classification, not binding, not liveness, not interop, not renaming. If the product starts
   asking a semantic question, it has already gone wrong.
2. **No bundler type escapes the adapter.** Nothing public and nothing persisted may contain
   one, or an upstream API change becomes a product-wide change.
3. **A build produces exactly one complete, parseable chunk** — or it is a failure.
4. **Every file the graph touched is remembered, including the tree-shaken ones**, or freshness
   is a lie.
5. **Bytes are fingerprinted as they are read**, never afterwards.
6. **The analyzer revision is bumped whenever a change can move a number.**
7. **Nothing waits for a build.** A cache hit never queues behind one, and a request never
   blocks on one: results are delivered as they land. A single slow import may cost its own
   number and nothing else.
8. **No failure path fabricates a symbol, measures partial code, or invents a size.**

And the five that exist because ignoring them produced the same defect seven times:

9. **A size exists if and only if a build succeeded.** There is no fallback number anywhere in
   the system.
10. **The question a consumer asks is "is there a size?" — never "is there an error?".** Sizes
    are optional and the compiler enforces the check. Invariant 9 is what makes this safe: a
    failed build has no size to misuse. *Any new code that reaches for `!result.error` to mean
    "usable" is reintroducing the bug.*
11. **A transient failure may never become durable.** Not cached, not persisted, not compared
    against a baseline, not turned into a pass/fail verdict. A *deterministic* failure may be
    cached — it is a fact about the bytes, and the cache is keyed by those bytes.
12. **An aggregate is only as complete as its inputs.** Any Loading or Unmeasured contributor
    makes the total a **floor**, and **no verdict may be drawn from a floor** — a budget judged
    against one is not failed, it is *not evaluated*. **A gate that cannot measure must never
    report success.** A deterministic `imprecise_assets` upper bound is cacheable but equally
    non-budgetable, because a verdict from it can falsely fail.
13. **A runtime is an artifact boundary.** Compressed bytes may be summed across one and never
    within one. Nothing is ever deduplicated across a runtime, because each runtime genuinely
    ships its own copy.

**The bundler is never asked to build one document's imports as separate bundles *within* a
runtime** — shared dependencies must be deduplicated by the bundler, not estimated afterwards.
Across runtimes, separate bundles are exactly right (invariant 13).

---

## 12. What it costs, in practice

Representative measurements from real packages:

| Import | Bytes reported | Modules rendered | Files loaded |
| --- | ---: | ---: | ---: |
| One parser export from a CSS toolkit | ~320 KB | 123 | 128 |
| One formatter from a date library | ~76 KB | 36 | **304** |
| One hook from React | ~54 KB | 3 | 3 |
| One function from a CJS utility library | ~489 KB | 1 | 1 |
| One function from its ESM twin | ~12 KB | 14 | **640** |

Two things worth reading twice.

**The date library loads 304 files and renders 36.** That gap is tree-shaking doing its job —
and every one of those 268 discarded files is still tracked for freshness, because editing one
could change what survives next time.

**The same function costs 489 KB from the CommonJS build and 12 KB from the ESM build.** CommonJS
cannot be tree-shaken; the whole library comes along. Surfacing that difference, in the editor,
before the code is written, is the entire reason this product exists.

Interactively, a cold build of a large package lands in tens of milliseconds; a cached answer is
effectively instant; and a full document of imports stays well inside the memory budget the
daemon is allowed to occupy while sharing a machine with the editor.

---

## 13. Precise contracts (for implementers)

The sections above are the *why*. This is the exact *what*: the machine-precise contracts the
narrative summarizes, gathered so the bundler lives in one place. `docs/ImportLens-SRS.md`
points here for all of it rather than restating any of it.

### 13.1 The virtual entry module (§4 in exact form)

For each cache miss the engine plugin serves an in-memory virtual entry under a synthetic id. Each
requested package maps to `import-lens:target/<index>` which resolves to the pre-resolved absolute
entry path (so the bundler never re-resolves the bare specifier). Every requested surface gets a
unique positional alias so strict entry signatures keep it alive, and names are emitted as
JSON-escaped string literals so a user-controlled name is never interpolated raw:

```javascript
// Named import (per requested name; string-literal names work identically)
export { "debounce" as __il_entry_0_export_0 } from "import-lens:target/0";

// Default import
export { default as __il_entry_0_default } from "import-lens:target/0";

// Namespace / dynamic / full-package use the escaping-namespace form, because
// `export * from` would drop the target's default export:
import * as __il_entry_0_namespace from "import-lens:target/0";
export { __il_entry_0_namespace };
```

Dynamic-import sizing maps to the full-package form (code splitting is disabled, so the measurement
stays one static chunk). Multi-import file sizing supplies all resolved requests as entries of one
build (indexes `0..n`) so shared dependencies link once.

### 13.2 Compression (§6 in exact form)

After codegen emits the minified string, the three compressions run in parallel (nested
`rayon::join`): **gzip level 6**, **brotli level 4**, **zstd level 3**, all over the minified string,
never the raw one. All three are collected before the response is sent.

### 13.3 The tree-shakeability threshold (§6 in exact form)

`truly_treeshakeable` is `false` when
`named_export_minified_size / full_package_minified_size > 0.95` (minified bytes, because minified
and compressed bytes are the user-facing surfaces). It is also `false` whenever the import is
**side-effectful** (FR-021: `sideEffects` absent, `true`, or a glob the *measured entry* matches),
because the comparison is only meaningful for a side-effect-free named import. A glob the entry does
*not* match (`["**/*.css"]` on a JS entry) is not side-effectful, so the flag is measured normally.
The full-package variant is a second build; if it fails, the flag degrades to `false` with a
diagnostic rather than failing the analysis.

### 13.4 The engine boundary contract (§2, §5 in exact form)

The engine adapter (`daemon/src/engine/`) is the only place permitted to name a Rolldown type; no
public or persistent type contains one (invariant 2). Callers submit a `BundleRequest` (entries with
pre-resolved `entry_path`, package root, and selection (named / default / namespace / full), plus
the runtime profile and purpose) and receive either a `BundleArtifact` or a typed `BundleFailure`.
**No `sideEffects` metadata crosses this boundary**: Rolldown reads the declaration itself from the
manifest the plugin supplies, and FR-021 makes it the sole authority on retention. Artifact
invariants:

- `code` is one complete, parseable, unminified ESM chunk.
- `loaded_paths` is every internal real file loaded during the scan (including tree-shaken ones),
  canonicalized, sorted, deduplicated.
- `contributions` is only rendered modules, using Rolldown's rendered lengths; pre-minification
  approximations, not required to sum to the chunk length.
- `exported_names` comes from the entry chunk's public export list, never a custom export walker.

**The reporting-only `sideEffects` match.** Recorded so it is not mistaken for dead code or for a
second bundler: the daemon *does* match `package.json#sideEffects` globs against the entry it
measures, but that match is **reporting-only and retention-neutral**: it never reaches Rolldown and
cannot change what is retained or what size is reported. It survives on the **successful measurement**
path, where it decides the `side_effects` badge the UI shows, because Rolldown 1.1.5 does not expose
its own retention decisions and there is no other way to tell the user whether the file they imported
is one the package declared effectful. The matcher itself is **not ours**: it is
`fast_glob::glob_match` (the crate `rolldown_common`/`rolldown_utils` match `sideEffects` with),
called with Rolldown's own pattern normalization mirrored around it and exact-pinned into the
coordinated compiler stack at the version Rolldown resolves. A lookalike matcher would be a way to
contradict the bundler whose retention we are describing ([ADR-0002](adr/0002-upstream-owns-everything-it-can-answer.md):
where upstream vendors a component, we use *that* component).

### 13.5 The analyzer revision

The authoritative value and its rationale live in `daemon/src/cache/key.rs` (`ANALYZER_REVISION`).
History: `graph2` (old custom engine) → `rolldown1` (Phase 3 cutover) → `rolldown2` (2026-07-12,
post-cutover correctness fixes) → `rolldown-1.1.x+3` (2026-07-15, the release-review fixes: the
Windows verbatim-path `sideEffects` bug behind `refractor`'s 3.7x under-report, the deleted
fabricator, per-runtime compression, deterministic failure-stage ranking, runtime-correct
enumeration). Format `<engine>-<minor line>.x+<revision>`: the patch is a wildcard so a Rolldown
patch that moves no numbers needs no bump; our own number-moving changes advance the counter.
