---
name: ts-ipc-client
description: "TypeScript IPC client over Unix sockets / Windows named pipes using MessagePack with 4-byte length-prefix framing. Use when implementing extension/src/ipc/ (FR-010, FR-011, FR-012, FR-013, FR-014, NFR-014b)."
---

# Instructions

The extension communicates with the Rust daemon exclusively over local IPC. You must implement a client with MessagePack encoding and length-prefix framing.

## 1. Socket Path — Window-Unique (NFR-014b)

The socket path MUST include a component unique to the VS Code window instance to prevent collisions when multiple windows are open:

```typescript
import * as net from "net";
import * as os from "os";
import * as path from "path";

// Use VSCODE_PID env var or generate a UUID at extension activation
const uniqueId = process.env.VSCODE_PID ?? crypto.randomUUID();

const socketPath =
  os.platform() === "win32"
    ? `\\\\.\\pipe\\import-lens-${uniqueId}`
    : path.join(os.tmpdir(), `import-lens-${uniqueId}.sock`);
```

## 2. Length-Prefix Framing (FR-011) — CRITICAL

Every message on the wire MUST be prefixed with a **4-byte big-endian unsigned integer** representing the byte length of the MessagePack payload.

```typescript
import { encode, decode } from "@msgpack/msgpack";

function sendMessage(socket: net.Socket, message: unknown): void {
  const payload = encode(message);
  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.byteLength, 0);
  socket.write(header);
  socket.write(
    Buffer.from(payload.buffer, payload.byteOffset, payload.byteLength),
  );
}
```

Reading messages requires buffering until the full length-prefixed frame is available:

```typescript
class FrameReader {
  private buffer = Buffer.alloc(0);

  feed(chunk: Buffer): unknown[] {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    const messages: unknown[] = [];

    while (this.buffer.length >= 4) {
      const len = this.buffer.readUInt32BE(0);
      if (this.buffer.length < 4 + len) break;

      const payload = this.buffer.subarray(4, 4 + len);
      messages.push(decode(payload));
      this.buffer = this.buffer.subarray(4 + len);
    }

    return messages;
  }
}
```

## 3. Connection Protocol

Upon successful connection, the client MUST send exactly ONE `HelloMessage` before any `BatchRequest`:

```typescript
const helloMessage = {
  type: "hello",
  version: 1,
  workspace_root: workspaceRootAbsolute,
  enable_disk_cache: config.get<boolean>("enableDiskCache", true),
  log_level: config.get<string>("logLevel", "error"),
};

sendMessage(client, helloMessage);
```

## 4. Request Cancellation (FR-013)

If a new debounce cycle fires before the previous response has been received, the previous request must be cancelled. When the stale response arrives, it must be discarded.

```typescript
let currentRequestId = 0;

function sendBatchRequest(imports: ImportRequest[]): void {
  currentRequestId++;
  const myId = currentRequestId;

  sendMessage(client, {
    version: 1,
    active_document_path: activeDocumentPath,
    imports,
  });

  // When response arrives, check if myId === currentRequestId
  // If not, discard the response
}
```

## 5. Socket Disconnect Handling (FR-014)

On disconnect, discard any stale MessagePack payloads in the receive buffer and wait for the next document change event to trigger a fresh request cycle. Do NOT attempt immediate reconnection.

```typescript
client.on("close", () => {
  frameReader = new FrameReader(); // discard stale buffer
  // Wait for next document change to trigger reconnect via daemon lifecycle
});
```

## Rules

- **Do NOT** use `JSON.stringify` or `JSON.parse` across the socket. MessagePack only.
- **Do NOT** use `decodeMultiStream` from `@msgpack/msgpack` — it does not handle length-prefix framing. Use the `FrameReader` pattern above.
- All imports from a single debounce cycle must be sent as a single `BatchRequest`, not one per import.
- Dispatch `CacheInvalidateMessage` from the file watcher (see `ts-vscode-workspace` skill).
- Terminate the socket connection cleanly on `deactivate()`.
