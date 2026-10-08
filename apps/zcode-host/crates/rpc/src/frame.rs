//! `SocketProtocol` 分帧（`packages/rpc/src/protocol.ts`）：type(1) id(4) ack(4) len(4)，大端。
//!
//! WebSocket 的消息边界不保证与帧对齐（TS 侧把收到的数据当字节流重组），所以读端按字节流处理。

pub const FRAME_HEADER: usize = 13;
pub const FRAME_REGULAR: u8 = 1;

pub fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER + message.len());
    out.push(FRAME_REGULAR);
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

#[derive(Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
    start: usize,
}

impl FrameReader {
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// 取出下一条完整的 Regular 消息；其他类型（Ack、KeepAlive 等）与 TS 一样丢弃。
    pub fn next_message(&mut self) -> Option<Vec<u8>> {
        loop {
            let available = &self.buffer[self.start..];
            if available.len() < FRAME_HEADER {
                self.compact();
                return None;
            }
            let mut length_bytes = [0u8; 4];
            length_bytes.copy_from_slice(&available[9..FRAME_HEADER]);
            let length = u32::from_be_bytes(length_bytes) as usize;
            if available.len() < FRAME_HEADER + length {
                self.compact();
                return None;
            }
            let kind = available[0];
            let body = available[FRAME_HEADER..FRAME_HEADER + length].to_vec();
            self.start += FRAME_HEADER + length;
            if kind == FRAME_REGULAR {
                return Some(body);
            }
        }
    }

    fn compact(&mut self) {
        if self.start > 0 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
    }
}
