//! 飞书长连接的帧格式（@larksuiteoapi/node-sdk 的 `pbbp2.Frame`，protobuf）。手写编解码，只覆盖
//! 这一个消息：
//!
//! ```text
//! Frame  { 1 SeqID uint64, 2 LogID uint64, 3 service int32, 4 method int32,
//!          5 headers repeated Header, 6 payloadEncoding string, 7 payloadType string,
//!          8 payload bytes, 9 LogIDNew string }
//! Header { 1 key string, 2 value string }
//! ```
use anyhow::{Result, bail};

/// `FrameType`。
pub const METHOD_CONTROL: i32 = 0;
pub const METHOD_DATA: i32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    pub seq_id: u64,
    pub log_id: u64,
    pub service: i32,
    pub method: i32,
    pub headers: Vec<(String, String)>,
    pub payload_encoding: Option<String>,
    pub payload_type: Option<String>,
    pub payload: Option<Vec<u8>>,
    pub log_id_new: Option<String>,
}

impl Frame {
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// 与 protobufjs 生成的 `Frame.encode` 相同：前四个标量字段总是写出，可选字段存在才写。
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_tag(&mut out, 1, 0);
        put_varint(&mut out, self.seq_id);
        put_tag(&mut out, 2, 0);
        put_varint(&mut out, self.log_id);
        put_tag(&mut out, 3, 0);
        put_varint(&mut out, self.service as i64 as u64);
        put_tag(&mut out, 4, 0);
        put_varint(&mut out, self.method as i64 as u64);
        for (key, value) in &self.headers {
            let mut header = Vec::new();
            put_bytes(&mut header, 1, key.as_bytes());
            put_bytes(&mut header, 2, value.as_bytes());
            put_bytes(&mut out, 5, &header);
        }
        if let Some(text) = &self.payload_encoding {
            put_bytes(&mut out, 6, text.as_bytes());
        }
        if let Some(text) = &self.payload_type {
            put_bytes(&mut out, 7, text.as_bytes());
        }
        if let Some(payload) = &self.payload {
            put_bytes(&mut out, 8, payload);
        }
        if let Some(text) = &self.log_id_new {
            put_bytes(&mut out, 9, text.as_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut frame = Frame::default();
        let mut reader = Reader { bytes, at: 0 };
        while !reader.done() {
            let (field, wire) = reader.tag()?;
            match (field, wire) {
                (1, 0) => frame.seq_id = reader.varint()?,
                (2, 0) => frame.log_id = reader.varint()?,
                (3, 0) => frame.service = reader.varint()? as i32,
                (4, 0) => frame.method = reader.varint()? as i32,
                (5, 2) => {
                    let mut header = Reader {
                        bytes: reader.bytes()?,
                        at: 0,
                    };
                    let (mut key, mut value) = (String::new(), String::new());
                    while !header.done() {
                        match header.tag()? {
                            (1, 2) => key = header.string()?,
                            (2, 2) => value = header.string()?,
                            (_, wire) => header.skip(wire)?,
                        }
                    }
                    frame.headers.push((key, value));
                }
                (6, 2) => frame.payload_encoding = Some(reader.string()?),
                (7, 2) => frame.payload_type = Some(reader.string()?),
                (8, 2) => frame.payload = Some(reader.bytes()?.to_vec()),
                (9, 2) => frame.log_id_new = Some(reader.string()?),
                (_, wire) => reader.skip(wire)?,
            }
        }
        Ok(frame)
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_tag(out: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(out, (u64::from(field) << 3) | u64::from(wire));
}

fn put_bytes(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_tag(out, field, 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let Some(&byte) = self.bytes.get(self.at) else {
                bail!("truncated varint");
            };
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("varint too long")
    }

    fn tag(&mut self) -> Result<(u32, u8)> {
        let tag = self.varint()?;
        Ok(((tag >> 3) as u32, (tag & 7) as u8))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = usize::try_from(self.varint()?)?;
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| anyhow::anyhow!("truncated field"))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => self.at += 8,
            2 => {
                self.bytes()?;
            }
            5 => self.at += 4,
            other => bail!("unsupported wire type {other}"),
        }
        if self.at > self.bytes.len() {
            bail!("truncated field");
        }
        Ok(())
    }
}
