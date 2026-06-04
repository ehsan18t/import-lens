import { decode, encode } from "@msgpack/msgpack";

const frameHeaderBytes = 4;
const maxFrameBytes = 32 * 1024 * 1024;

export const encodeFrame = (message: unknown): Buffer => {
  const payload = Buffer.from(encode(message));
  if (payload.length > maxFrameBytes) {
    throw new RangeError(`IPC frame is too large: ${payload.length} bytes`);
  }

  const frame = Buffer.allocUnsafe(frameHeaderBytes + payload.length);
  frame.writeUInt32BE(payload.length, 0);
  payload.copy(frame, frameHeaderBytes);
  return frame;
};

export class FrameDecoder {
  #chunks: Buffer[] = [];
  #bufferedBytes = 0;
  #readOffset = 0;

  push(chunk: Buffer): unknown[] {
    if (chunk.length > 0) {
      this.#chunks.push(chunk);
      this.#bufferedBytes += chunk.length;
    }

    const messages: unknown[] = [];

    while (this.#bufferedBytes >= frameHeaderBytes) {
      const payloadLength = this.#peekBytes(frameHeaderBytes).readUInt32BE(0);
      if (payloadLength > maxFrameBytes) {
        this.reset();
        throw new RangeError(`IPC frame is too large: ${payloadLength} bytes`);
      }

      const frameLength = frameHeaderBytes + payloadLength;

      if (this.#bufferedBytes < frameLength) {
        break;
      }

      this.#readBytes(frameHeaderBytes);
      const payload = this.#readBytes(payloadLength);
      messages.push(decode(payload));
    }

    return messages;
  }

  reset(): void {
    this.#chunks = [];
    this.#bufferedBytes = 0;
    this.#readOffset = 0;
  }

  #peekBytes(length: number): Buffer {
    const first = this.#chunks[0];
    if (!first) {
      return Buffer.alloc(0);
    }

    const firstAvailable = first.length - this.#readOffset;
    if (firstAvailable >= length) {
      return first.subarray(this.#readOffset, this.#readOffset + length);
    }

    const bytes = Buffer.allocUnsafe(length);
    let written = 0;
    let chunkIndex = 0;
    let offset = this.#readOffset;

    while (written < length) {
      const chunk = this.#chunks[chunkIndex];
      const available = chunk.length - offset;
      const toCopy = Math.min(length - written, available);
      chunk.copy(bytes, written, offset, offset + toCopy);
      written += toCopy;
      chunkIndex++;
      offset = 0;
    }

    return bytes;
  }

  #readBytes(length: number): Buffer {
    const first = this.#chunks[0];
    if (first) {
      const firstAvailable = first.length - this.#readOffset;
      if (firstAvailable >= length) {
        const bytes = first.subarray(this.#readOffset, this.#readOffset + length);
        this.#advance(length);
        return bytes;
      }
    }

    const bytes = Buffer.allocUnsafe(length);
    let written = 0;

    while (written < length) {
      const chunk = this.#chunks[0];
      const available = chunk.length - this.#readOffset;
      const toCopy = Math.min(length - written, available);
      chunk.copy(bytes, written, this.#readOffset, this.#readOffset + toCopy);
      written += toCopy;
      this.#advance(toCopy);
    }

    return bytes;
  }

  #advance(length: number): void {
    this.#bufferedBytes -= length;
    this.#readOffset += length;

    while (this.#chunks.length > 0 && this.#readOffset >= this.#chunks[0].length) {
      this.#readOffset -= this.#chunks[0].length;
      this.#chunks.shift();
    }
  }
}
