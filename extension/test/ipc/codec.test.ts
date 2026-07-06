import assert from "node:assert/strict";
import test from "node:test";
import { encodeFrame, FrameDecoder } from "../../src/ipc/codec.js";
import type { CacheRemoveRequest } from "../../src/ipc/protocol.js";

test("encodeFrame prefixes MessagePack payloads with a big-endian length", () => {
  const frame = encodeFrame({ type: "shutdown" });
  const payloadLength = frame.readUInt32BE(0);

  assert.equal(payloadLength, frame.length - 4);
  assert.ok(frame.length > 4);
});

test("FrameDecoder returns complete messages and buffers partial frames", () => {
  const decoder = new FrameDecoder();
  const first = encodeFrame({ type: "cache_invalidate", package: "react" });
  const second = encodeFrame({ type: "shutdown" });
  const splitPoint = 5;

  assert.deepEqual(decoder.push(first.subarray(0, splitPoint)), []);

  const decoded = decoder.push(Buffer.concat([first.subarray(splitPoint), second]));

  assert.deepEqual(decoded, [{ type: "cache_invalidate", package: "react" }, { type: "shutdown" }]);
});

test("FrameDecoder rejects oversized frames before buffering payload", () => {
  const decoder = new FrameDecoder();
  const header = Buffer.alloc(4);
  header.writeUInt32BE(32 * 1024 * 1024 + 1, 0);

  assert.throws(() => decoder.push(header), /too large/u);
});

test("cache_remove registry scope survives a MessagePack codec round trip", () => {
  const decoder = new FrameDecoder();
  const request: CacheRemoveRequest = {
    type: "cache_remove",
    version: 7,
    request_id: 71,
    scope: "registry",
  };

  const [decoded] = decoder.push(encodeFrame(request));

  assert.deepEqual(decoded, request);
});
