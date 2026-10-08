// Rust Host ⇄ 远端连接器的帧协议（docs/specs/rust-host-remote.md §2.1）。
//
// 连接器的 stdin / stdout 都是二进制帧：stream u8 | len u32BE | payload。
//   stream 0：控制 JSON（Host → 连接器：connect、dispose；连接器 → Host：log、connected、failed、closed）
//   stream 1：握手之后远端 zcode-server 的 stdio 字节，双向原样转发
//   stream 2：连接器自身的 @zcode/rpc 服务，每帧一条完整消息
export const REMOTE_CONNECTOR_STREAM = {
  control: 0,
  server: 1,
  backend: 2,
} as const;

export type RemoteConnectorStream =
  (typeof REMOTE_CONNECTOR_STREAM)[keyof typeof REMOTE_CONNECTOR_STREAM];

const FRAME_HEADER_BYTES = 5;
/** 与 Rust 侧相同的单帧上限，防止损坏的长度字段申请超大缓冲。 */
const MAX_FRAME_BYTES = 64 * 1024 * 1024;

export function encodeRemoteConnectorFrame(stream: RemoteConnectorStream, payload: Uint8Array): Buffer {
  const frame = Buffer.allocUnsafe(FRAME_HEADER_BYTES + payload.byteLength);
  frame.writeUInt8(stream, 0);
  frame.writeUInt32BE(payload.byteLength, 1);
  frame.set(payload, FRAME_HEADER_BYTES);
  return frame;
}

export function encodeRemoteConnectorControl(message: Record<string, unknown>): Buffer {
  return encodeRemoteConnectorFrame(
    REMOTE_CONNECTOR_STREAM.control,
    Buffer.from(JSON.stringify(message), "utf8"),
  );
}

/** 按字节流重组帧；返回完整帧，剩余字节留到下一次。 */
export class RemoteConnectorFrameReader {
  private buffer: Buffer = Buffer.alloc(0);

  push(chunk: Uint8Array): Array<{ stream: number; payload: Buffer }> {
    this.buffer = this.buffer.length === 0 ? Buffer.from(chunk) : Buffer.concat([this.buffer, chunk]);
    const frames: Array<{ stream: number; payload: Buffer }> = [];
    let offset = 0;
    while (this.buffer.length - offset >= FRAME_HEADER_BYTES) {
      const stream = this.buffer.readUInt8(offset);
      const length = this.buffer.readUInt32BE(offset + 1);
      if (length > MAX_FRAME_BYTES) {
        throw new Error(`remote connector frame exceeds ${MAX_FRAME_BYTES} bytes`);
      }
      if (this.buffer.length - offset < FRAME_HEADER_BYTES + length) break;
      const start = offset + FRAME_HEADER_BYTES;
      frames.push({ stream, payload: this.buffer.subarray(start, start + length) });
      offset = start + length;
    }
    this.buffer = offset === 0 ? this.buffer : this.buffer.subarray(offset);
    return frames;
  }
}
