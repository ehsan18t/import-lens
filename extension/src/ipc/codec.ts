import { decode, encode } from "@msgpack/msgpack";

const frameHeaderBytes = 4;

export const encodeFrame = (message: unknown): Buffer => {
  const payload = Buffer.from(encode(message));
  const frame = Buffer.allocUnsafe(frameHeaderBytes + payload.length);
  frame.writeUInt32BE(payload.length, 0);
  payload.copy(frame, frameHeaderBytes);
  return frame;
};

export class FrameDecoder {
  #buffer: Buffer = Buffer.alloc(0);

  push(chunk: Buffer): unknown[] {
    this.#buffer = Buffer.concat([this.#buffer, chunk]);
    const messages: unknown[] = [];

    while (this.#buffer.length >= frameHeaderBytes) {
      const payloadLength = this.#buffer.readUInt32BE(0);
      const frameLength = frameHeaderBytes + payloadLength;

      if (this.#buffer.length < frameLength) {
        break;
      }

      const payload = this.#buffer.subarray(frameHeaderBytes, frameLength);
      messages.push(decode(payload));
      this.#buffer = this.#buffer.subarray(frameLength);
    }

    return messages;
  }

  reset(): void {
    this.#buffer = Buffer.alloc(0);
  }
}

