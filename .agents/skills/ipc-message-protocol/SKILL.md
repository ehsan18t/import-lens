---
name: ipc-message-protocol
description: "Complete IPC message schemas, length-prefix framing, protocol versioning, and MessagePack encoding for all 5 message types. Use when implementing protocol.ts and protocol.rs (FR-010–FR-014, NFR-018)."
---

# Instructions

All communication between the TypeScript extension host and the Rust daemon uses MessagePack-encoded messages over a local IPC socket with 4-byte length-prefix framing.

## 1. Wire Format — Length-Prefix Framing

Every message on the wire follows this format:

```
┌──────────────┬────────────────────────────┐
│ 4 bytes (BE) │ N bytes (MessagePack)       │
│ payload len  │ payload                     │
└──────────────┴────────────────────────────┘
```

The 4-byte header is a big-endian unsigned 32-bit integer containing the byte length of the MessagePack payload that follows.

## 2. Message Types

There are exactly 5 message types. All are sent client→daemon except `BatchResponse`.

### HelloMessage (client → daemon)

Sent exactly ONCE immediately after connection. The daemon MUST NOT process any other message until a valid Hello is received.

```typescript
interface HelloMessage {
  type: "hello";
  version: number; // Protocol version, currently 1
  workspace_root: string; // Absolute path to the workspace root
  enable_disk_cache: boolean; // From importLens.enableDiskCache setting
  log_level: "error" | "warn" | "info" | "debug";
}
```

### BatchRequest (client → daemon)

```typescript
interface BatchRequest {
  version: number; // Protocol version, currently 1
  active_document_path: string; // Absolute path to the file being edited
  imports: ImportRequest[];
}

interface ImportRequest {
  specifier: string; // Raw import specifier, e.g. "lodash-es"
  package: string; // Resolved package name
  version: string; // Installed version, e.g. "4.17.21"
  named: string[]; // Named exports; empty for default/namespace/dynamic
  import_kind: "named" | "default" | "namespace" | "dynamic";
}
```

> [!IMPORTANT]
> `active_document_path` is the absolute path to the file being edited, NOT the workspace root. `oxc_resolver` starts upward traversal from this path to correctly resolve nested `node_modules` in monorepos.

### BatchResponse (daemon → client)

```typescript
interface BatchResponse {
  version: number;
  imports: ImportResult[];
}

interface ImportResult {
  specifier: string;
  raw_bytes: number;
  minified_bytes: number;
  gzip_bytes: number;
  brotli_bytes: number;
  zstd_bytes: number;
  cache_hit: boolean;
  side_effects: boolean;
  truly_treeshakeable: boolean;
  is_cjs: boolean;
  error: string | null;
}
```

### CacheInvalidateMessage (client → daemon)

Sent when the file watcher detects a change in `node_modules`.

```typescript
interface CacheInvalidateMessage {
  type: "cache_invalidate";
  package: string; // Including scope for scoped packages, e.g. "@babel/core"
}
```

### ShutdownMessage (client → daemon)

Sent on extension deactivation. Daemon must flush caches and exit.

```typescript
interface ShutdownMessage {
  type: "shutdown";
}
```

## 3. Protocol Versioning (NFR-018)

- Both `BatchRequest` and `BatchResponse` include a `version` field (integer, currently `1`).
- The daemon MUST reject requests with an unrecognized version number and respond with an error batch response.
- This enables future protocol evolution without breaking older clients.

## 4. Rust Deserialization

On the Rust side, use `rmp-serde` with a tagged enum:

```rust
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Hello { version: u32, workspace_root: String, enable_disk_cache: bool, log_level: String },
    CacheInvalidate { package: String },
    Shutdown {},
}

// BatchRequest is deserialized separately (no "type" tag — it uses the version field)
#[derive(Deserialize)]
struct BatchRequest {
    version: u32,
    active_document_path: String,
    imports: Vec<ImportRequest>,
}
```

## Rules

- Use `@msgpack/msgpack` on the TypeScript side and `rmp-serde` on the Rust side.
- Do NOT use JSON anywhere in the IPC layer.
- All imports from a single debounce cycle go in ONE `BatchRequest`.
- The daemon must reject unknown `version` values with an error response.
