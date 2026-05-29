import assert from "node:assert/strict";
import test from "node:test";
import { FrameDecoder, encodeFrame } from "../../src/ipc/codec.js";

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

  assert.deepEqual(decoded, [
    { type: "cache_invalidate", package: "react" },
    { type: "shutdown" },
  ]);
});

