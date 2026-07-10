# Import Lens Logging Policy

Normative rules for extension host and daemon logging. The VS Code output channel is the primary user-facing log surface (FR-040, FR-041).

## Levels

| Level | Use for | Must not use for |
|-------|---------|------------------|
| **error** | Daemon hash mismatch, crash-degraded mode entry, unrecoverable security or integrity failures | Per-import sizing fallbacks that still return usable bytes |
| **warn** | Daemon or IPC unavailable, startup failure, final analysis failure, cache flush failure, globalState write failure, registry fetch exhausted after retries | Successful low-confidence import results (FR-039c) |
| **info** | Activation, daemon ready, cache invalidation, config changes, user-triggered commands | Per-import partial streaming frames |
| **debug** | IPC request summaries, batch partial frames, registry fetch attempts, prewarm, import diagnostic detail | Default user sessions at `importLens.logLevel: info` |

## FR-039c deduplication

- **Warn once** per `(request_id, specifier, error)` when `ImportResult.error` is set and no measured size exists, or when the daemon returns no result for a scheduled import.
- **Debug once** per `(request_id, specifier)` for diagnostics, confidence reasons, and low-confidence fallback detail on otherwise successful results.
- Do not warn for imports that produced a usable size with low confidence; surface detail in hover, report, copied diagnostics, and debug logs only.

## Message format

### Extension host

```
2026-06-15T12:00:00.000Z [INFO] [listener] req=42 message text
```

Optional context segments: `[component]`, `req=<id>`, `uri=<path>`, `pkg=<name>`.

### Daemon (structured, host-parseable)

```
[import-lens-daemon] 2026-06-15T12:00:00.000Z [WARN] [cache] message text
```

- **stdout**: `info`, `debug`
- **stderr**: `warn`, `error`

The extension host parses structured daemon lines and applies `importLens.logLevel` before writing to the Import Lens output channel. Unparsed stdout lines map to info; unparsed stderr lines map to warn (FR-015a compatibility).

## Configuration

- Extension: `importLens.logLevel` (`error` | `warn` | `info` | `debug`, default `info`)
- Daemon: `HelloMessage.log_level` — set once per session from the extension setting

Both sides filter locally before emitting. Opening the output channel always writes a log-level breadcrumb (FR-040).
