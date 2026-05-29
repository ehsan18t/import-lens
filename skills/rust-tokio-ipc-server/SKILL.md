---
name: rust-tokio-ipc-server
description: "Rust async IPC server using Tokio, rmp-serde MessagePack, and 4-byte length-prefix framing. Use when implementing daemon/src/ipc/ (FR-010, FR-011, NFR-014, NFR-014b)."
---

# Instructions

The daemon listens on a Unix socket (macOS/Linux) or Named Pipe (Windows) for incoming MessagePack packets with length-prefix framing.

## 1. Socket Path — Uniqueness Required (NFR-014b)

The socket path MUST include a window-unique identifier to prevent collisions when multiple VS Code windows are open:

```rust
// Unix: /tmp/import-lens-<unique_id>.sock
// Windows: \\.\pipe\import-lens-<unique_id>

// The unique_id comes from the HelloMessage's workspace_root hash
// or a UUID generated at extension activation and passed via command-line arg.
```

## 2. Length-Prefix Framing (FR-011) — CRITICAL

Every message on the wire must be prefixed with a **4-byte big-endian unsigned integer** representing the byte length of the MessagePack payload that follows.

```rust
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Reading a message:
async fn read_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

// Writing a message:
async fn write_message<W: AsyncWriteExt + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    let len = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}
```

## 3. Listener Setup

```rust
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(windows)]
use tokio::net::windows::named_pipe::ServerOptions;
```

For Unix, explicitly set the socket file permissions to `0600` (user read/write only) after binding (NFR-014).

```rust
#[cfg(unix)]
{
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
}
```

## 4. Deserialization

Use `rmp-serde` (v1.3.1) to decode the MessagePack payload into typed messages.

```rust
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Hello {
        version: u32,
        workspace_root: String,
        enable_disk_cache: bool,
        log_level: String,
    },
    BatchRequest {
        version: u32,
        active_document_path: String,
        imports: Vec<ImportRequest>,
    },
    CacheInvalidate {
        package: String,
    },
    Shutdown {},
}
```

## 5. Protocol Versioning (NFR-018)

Both `BatchRequest` and `BatchResponse` include a `version` field (integer, currently `1`). The daemon MUST reject requests with an unrecognised version number and respond with an error.

## 6. The Graceful Shutdown Protocol (FR-038) — 3-Step Sequence

When you receive the `Shutdown` message:

1. Stop the listener from accepting new connections.
2. Flush all pending `papaya` entries to `redb`.
3. Close the `redb` database.
4. Remove the Unix socket file (macOS/Linux) or release the named pipe (Windows).
5. Exit the process cleanly within 5 seconds.

The extension host manages the escalation: Shutdown → 5s → SIGTERM → 2s → SIGKILL (Unix). On Windows, `TerminateProcess` is used after timeout. The daemon itself only needs to handle the `Shutdown` message gracefully.

## Rules

- **Do not** process `BatchRequest` messages before a successful `HelloMessage` has been received.
- The `rmp-serde` deserializer must handle streaming input bytes correctly without blocking the Tokio runtime.
- Use `tokio` features: `rt-multi-thread`, `net`, `io-util`, `macros`.
