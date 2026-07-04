---
name: ts-daemon-lifecycle
description: "Spawning the Rust daemon with binary integrity verification and graceful 3-step shutdown (Shutdown→SIGTERM→SIGKILL). Use when implementing extension/src/extension.ts and related lifecycle code (FR-015, FR-038, NFR-014a)."
---

# Instructions

The extension must spawn the native daemon, verify its integrity, and guarantee a graceful shutdown.

## 1. Binary Integrity Verification (NFR-014a) — CRITICAL

Before spawning the daemon, the extension host MUST:

1. Compute the SHA-256 hash of the daemon executable.
2. Compare it against a known-good hash embedded in the extension package (e.g., in a `dist/bin/<platform>/sha256` file or hardcoded in the build).
3. If the hash does NOT match, refuse to spawn, log a security warning to the `ImportLens` output channel, and enter degraded mode.

```typescript
import * as crypto from "crypto";
import * as fs from "fs";

async function verifyBinaryIntegrity(
  binaryPath: string,
  expectedHash: string,
): Promise<boolean> {
  const fileBuffer = await fs.promises.readFile(binaryPath);
  const hash = crypto.createHash("sha256").update(fileBuffer).digest("hex");
  return hash === expectedHash;
}
```

## 2. Spawning

Spawn the correct binary per the current OS/architecture. The binary is in the extension's `dist/bin/<platform>/` directory.

```typescript
import { spawn, ChildProcess } from "child_process";

const daemon: ChildProcess = spawn(binaryPath, [socketPath], {
  stdio: "ignore", // prevent stdout pollution
  detached: false,
});

daemon.on("error", (err) => {
  // Failed to spawn — enter degraded mode
});
```

## 3. Graceful Shutdown (FR-038) — 3-Step Escalation

When the extension deactivates (`deactivate()`), follow this exact sequence:

1. **Step 1 — IPC Shutdown**: Send the `Shutdown` message over the IPC socket. Wait up to 5 seconds for the daemon to exit cleanly.
2. **Step 2 — SIGTERM** (Unix) / **TerminateProcess** (Windows): If the daemon has not exited after 5 seconds, send `SIGTERM` (Unix) or call `TerminateProcess` (Windows). Wait an additional 2 seconds.
3. **Step 3 — SIGKILL** (Unix only): If the daemon STILL has not exited after the additional 2 seconds, send `SIGKILL` to forcefully terminate it. (`SIGTERM` can be caught or ignored; `SIGKILL` cannot.) On Windows, `TerminateProcess` is already unconditional — no second step needed.

```typescript
export async function deactivate(): Promise<void> {
  // Step 1: IPC Shutdown
  ipcClient.send({ type: "shutdown" });

  const exited = await waitForExit(daemon, 5000);
  if (exited) return;

  // Step 2: SIGTERM (Unix) / TerminateProcess (Windows)
  daemon.kill("SIGTERM");
  const exitedAfterTerm = await waitForExit(daemon, 2000);
  if (exitedAfterTerm) return;

  // Step 3: SIGKILL (Unix only)
  if (process.platform !== "win32") {
    daemon.kill("SIGKILL");
  }
}
```

## 4. Crash Recovery with Exponential Backoff (FR-015)

If the daemon crashes, the extension must:

1. Wait 1 second, then attempt restart.
2. On subsequent failures, apply exponential backoff: 1s, 2s, 4s, 8s, capped at 30s.
3. After 3 consecutive failures within 60 seconds, enter degraded mode and display a status bar notification.

```typescript
let crashCount = 0;
let lastCrashTime = 0;

daemon.on("exit", (code) => {
  if (code !== 0 && !intentionalShutdown) {
    const now = Date.now();
    if (now - lastCrashTime < 60_000) {
      crashCount++;
    } else {
      crashCount = 1;
    }
    lastCrashTime = now;

    if (crashCount >= 3) {
      enterDegradedMode("Daemon crashed 3 times within 60s");
      return;
    }

    const delay = Math.min(1000 * Math.pow(2, crashCount - 1), 30_000);
    setTimeout(() => restartDaemon(), delay);
  }
});
```

## Rules

- For VS Code for the Web (where `spawn` is unavailable), fail gracefully to Tier 3 Degraded Mode immediately.
- Do NOT skip the binary hash verification step. It prevents execution of tampered binaries.
- The socket path must include a window-unique identifier (see `ts-ipc-client` skill).
