# One CPU pool, idle reclaim, and builds only for visible documents

The daemon's resident memory is mostly Retained Memory, not data. On a 60-file workspace importing 25 packages, Linux sat at 226 MB idle with 14 to 46 MB of live heap, and Windows at 383 MB working set with 36 MB live. About 185 MB of the Linux figure was allocator arenas holding freed pages. That retention scales with thread count: every thread keeps its own mimalloc heap, and a page whose blocks were freed by another thread is only reclaimed when its owner thread next allocates or collects. Idle pool threads never do either.

The daemon ran eight separate pools: the global Rayon pool, prewarm, report, asset and registry pools, the IPC runtime and its blocking pool, and the engine runtime with an uncapped blocking pool. Rolldown reads every module with `spawn_blocking` on the engine runtime, so one build grew that blocking pool to 80 to 150 threads, each keeping a heap until a 10 second keep-alive expired.

## Decisions

1. **The engine runtime's blocking pool is capped at its worker count.** Its only blocking work is Rolldown's file reads, which are short; queueing them costs nothing measurable (warm rounds measured 9.5 s capped against 10.5 s uncapped) and removes the per-round growth. Nothing on that runtime may call `block_in_place`: against a capped pool it can deadlock.
2. **One CPU pool replaces the global, prewarm and report pools.** Interactive work runs first; background work (prewarm, workspace reports) is admitted through a lane of half the pool's width, so it can never occupy the whole pool, and a report splits its files into at most four pieces. The two asset-processing threads stay: a caller waits for asset work with a deadline, and on a shared pool an admitted job could queue past it and turn into an Unmeasured result under load. The isolation that protects the active file was already enforced at the engine (background holds at most one of the two build permits); dedicated threads per pool bought nothing that gate does not, and each cost a permanent heap. Registry refresh, which is network I/O, runs on the IPC blocking pool behind its existing concurrency cap.
3. **Every long-lived thread reclaims its heap when it goes idle**, through one mechanism: Tokio's park hook on both runtimes and a broadcast on the shared pool when its work drains.
4. **Builds run only for Visible Documents.** The extension sends `visible_documents` (protocol 8) whenever the set changes, and analyzes a document only when it is visible. The daemon cancels the queued, not-yet-started builds of every document outside the set; builds already inside Rolldown finish and are cached, because their result is reused when the user returns or another file imports the package.
5. **No RSS cap scales with the project.** A large project holds more live data, and capping it would fight the cache. Retention is gated instead: after a heavy soak and a full cache clear, where live data is near zero, resident memory must stay under a fixed bound on Windows and Linux.

## Consequences

A new thread pool is a new permanent heap and must say why the shared pool cannot carry its work. A new `spawn_blocking` site on the engine runtime is bounded by decision 1; a `block_in_place` there is a defect.
