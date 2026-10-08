//! V4 物理帧重组（packages/shared/src/zcode-protocol-v4/wire-reassembly.ts、wire-binary.ts）。
//!
//! 只给 Host 内部消费者（sessions-index 观察者）用；转发给 Renderer 的帧不在 Host 重组。
//! CLI 经同一条有序 stdio 连续写出一个逻辑帧的全部分片，所以这里按路由（topic + subscriptionId）
//! 只保留一个进行中的组装：
//! - ordinal 不大于已完成的 ordinal：丢弃（迟到或重复）；
//! - 上一帧还没收齐就来了新的逻辑帧：报 fault，由消费者 resync；
//! - 收齐后校验字节数、CRC32、UTF-8、JSON 与 envelope，任一失败都报 fault。
use std::collections::HashMap;

use serde_json::Value;

const MAX_LOGICAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FRAGMENTS: u64 = 1024;
const MAX_ROUTES: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum Assembled {
    /// 一个完整的逻辑帧（`{topic, subscriptionId, fromSeq, toSeq, payload, ...}`）。
    Frame {
        delivery_kind: String,
        frame: Value,
    },
    /// 分片未收齐，或被丢弃的迟到帧。
    Pending,
    Fault(&'static str),
}

struct Assembly {
    logical_frame_id: String,
    ordinal: u64,
    delivery_kind: String,
    count: usize,
    logical_bytes: usize,
    checksum: String,
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
}

#[derive(Default)]
struct Route {
    settled: u64,
    current: Option<Assembly>,
}

#[derive(Default)]
pub struct Assembler {
    routes: HashMap<(String, String), Route>,
}

/// 与 `crc32WireBytes` 相同：IEEE CRC32，8 位小写十六进制。
pub fn crc32_hex(bytes: &[u8]) -> String {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
        }
    }
    format!("{:08x}", crc ^ 0xffff_ffff)
}

/// 标准 base64（必须带填充、长度是 4 的倍数且至少 4），非法输入返回 None。
pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() < 4 || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let chunks = bytes.len() / 4;
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index + 1 == chunks;
        let padding = match (chunk[2], chunk[3]) {
            (b'=', b'=') if last => 2,
            (_, b'=') if last && chunk[2] != b'=' => 1,
            _ => 0,
        };
        let a = value(chunk[0])?;
        let b = value(chunk[1])?;
        let c = if padding == 2 { 0 } else { value(chunk[2])? };
        let d = if padding >= 1 { 0 } else { value(chunk[3])? };
        let combined = (a << 18) | (b << 12) | (c << 6) | d;
        out.push((combined >> 16) as u8);
        if padding < 2 {
            out.push((combined >> 8) as u8);
        }
        if padding < 1 {
            out.push(combined as u8);
        }
    }
    Some(out)
}

fn envelope_matches(frame: &Value, topic: &str, subscription_id: &str) -> bool {
    frame["topic"] == topic && frame["subscriptionId"] == subscription_id
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 订阅换了或 resync 之后，丢弃该路由的组装状态。
    pub fn reset(&mut self) {
        self.routes.clear();
    }

    pub fn accept(&mut self, wire: &Value) -> Assembled {
        let text = |key: &str| wire[key].as_str().unwrap_or_default().to_owned();
        let (topic, subscription_id) = (text("topic"), text("subscriptionId"));
        let Some(ordinal) = wire["logicalFrameOrdinal"]
            .as_u64()
            .filter(|ordinal| *ordinal >= 1)
        else {
            return Assembled::Fault("proto.frameAssemblyMetadataMismatch");
        };
        if self.routes.len() >= MAX_ROUTES
            && !self
                .routes
                .contains_key(&(topic.clone(), subscription_id.clone()))
        {
            self.routes.clear();
        }
        let route = self
            .routes
            .entry((topic.clone(), subscription_id.clone()))
            .or_default();
        if ordinal <= route.settled {
            return Assembled::Pending;
        }
        let delivery_kind = text("deliveryKind");
        match wire["kind"].as_str() {
            Some("complete") => {
                if route.current.take().is_some() {
                    return Assembled::Fault("proto.frameAssemblyIncomplete");
                }
                route.settled = ordinal;
                let frame = wire["frame"].clone();
                if !envelope_matches(&frame, &topic, &subscription_id) {
                    return Assembled::Fault("proto.frameAssemblyMetadataMismatch");
                }
                Assembled::Frame {
                    delivery_kind,
                    frame,
                }
            }
            Some("fragment") => {
                let result = Self::accept_fragment(route, wire, ordinal, delivery_kind);
                if let Assembled::Frame { frame, .. } = &result
                    && !envelope_matches(frame, &topic, &subscription_id)
                {
                    return Assembled::Fault("proto.frameAssemblyMetadataMismatch");
                }
                result
            }
            _ => Assembled::Fault("proto.frameAssemblyMetadataMismatch"),
        }
    }

    fn accept_fragment(
        route: &mut Route,
        wire: &Value,
        ordinal: u64,
        delivery_kind: String,
    ) -> Assembled {
        let mismatch = Assembled::Fault("proto.frameAssemblyMetadataMismatch");
        let (Some(count), Some(logical_bytes), Some(index)) = (
            wire["fragmentCount"].as_u64(),
            wire["logicalBytes"].as_u64(),
            wire["fragmentIndex"].as_u64(),
        ) else {
            return mismatch;
        };
        let logical_frame_id = wire["logicalFrameId"].as_str().unwrap_or_default();
        let checksum = wire["checksum"]["value"].as_str().unwrap_or_default();
        if wire["checksum"]["algorithm"] != "crc32" || count == 0 || index >= count {
            return mismatch;
        }
        if count > MAX_FRAGMENTS || count > logical_bytes.max(1) {
            return Assembled::Fault("proto.frameFragmentCountExceeded");
        }
        if logical_bytes > MAX_LOGICAL_BYTES {
            return Assembled::Fault("proto.frameAssemblyTooLarge");
        }
        if let Some(current) = &route.current
            && (current.logical_frame_id != logical_frame_id || current.ordinal != ordinal)
        {
            route.current = None;
            return Assembled::Fault("proto.frameAssemblyIncomplete");
        }
        let assembly = route.current.get_or_insert_with(|| Assembly {
            logical_frame_id: logical_frame_id.to_owned(),
            ordinal,
            delivery_kind,
            count: count as usize,
            logical_bytes: logical_bytes as usize,
            checksum: checksum.to_owned(),
            parts: vec![None; count as usize],
            received: 0,
        });
        if assembly.count != count as usize
            || assembly.logical_bytes != logical_bytes as usize
            || assembly.checksum != checksum
        {
            route.current = None;
            return mismatch;
        }
        let Some(decoded) = wire["dataBase64"].as_str().and_then(decode_base64) else {
            route.current = None;
            return Assembled::Fault("proto.frameAssemblyInvalidBase64");
        };
        let slot = &mut assembly.parts[index as usize];
        if let Some(previous) = slot {
            if *previous != decoded {
                route.current = None;
                return Assembled::Fault("proto.frameAssemblyFragmentConflict");
            }
            return Assembled::Pending;
        }
        assembly.received += decoded.len();
        if assembly.received > assembly.logical_bytes {
            route.current = None;
            return Assembled::Fault("proto.frameAssemblyLengthMismatch");
        }
        *slot = Some(decoded);
        if assembly.parts.iter().any(Option::is_none) {
            return Assembled::Pending;
        }
        let Some(assembly) = route.current.take() else {
            return Assembled::Pending;
        };
        route.settled = ordinal;
        if assembly.received != assembly.logical_bytes {
            return Assembled::Fault("proto.frameAssemblyLengthMismatch");
        }
        let logical: Vec<u8> = assembly.parts.into_iter().flatten().flatten().collect();
        if crc32_hex(&logical) != assembly.checksum {
            return Assembled::Fault("proto.frameAssemblyChecksumMismatch");
        }
        let Ok(text) = String::from_utf8(logical) else {
            return Assembled::Fault("proto.frameAssemblyInvalidUtf8");
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(frame) => Assembled::Frame {
                delivery_kind: assembly.delivery_kind,
                frame,
            },
            Err(_) => Assembled::Fault("proto.frameAssemblyInvalidJson"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zcode_host_kit::base64_encode;

    #[test]
    fn crc32_and_base64_match_typescript() {
        // crc32WireBytes(new TextEncoder().encode("hello")) === "3610a686"
        assert_eq!(crc32_hex(b"hello"), "3610a686");
        assert_eq!(decode_base64("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64("aGk="), Some(b"hi".to_vec()));
        assert_eq!(decode_base64("aGVs"), Some(b"hel".to_vec()));
        assert_eq!(decode_base64("aGV"), None);
        assert_eq!(decode_base64("a=Vs"), None);
    }

    fn fragments(frame: &Value, ordinal: u64, size: usize) -> Vec<Value> {
        let bytes = frame.to_string().into_bytes();
        let checksum = crc32_hex(&bytes);
        let chunks: Vec<&[u8]> = bytes.chunks(size).collect();
        chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| {
                json!({
                    "wireVersion": 3, "kind": "fragment", "deliveryKind": "online",
                    "logicalFrameId": format!("f{ordinal}"), "logicalFrameOrdinal": ordinal,
                    "topic": "sessions-index/w", "subscriptionId": "s",
                    "fragmentIndex": index, "fragmentCount": chunks.len(),
                    "logicalBytes": bytes.len(),
                    "checksum": {"algorithm": "crc32", "value": checksum},
                    "dataBase64": base64_encode(chunk),
                })
            })
            .collect()
    }

    #[test]
    fn reassembles_fragments_and_drops_stale_ordinals() {
        let frame = json!({"topic": "sessions-index/w", "subscriptionId": "s", "fromSeq": 0, "toSeq": 1, "payload": {"kind": "snapshot", "snapshot": {"sessions": ["x".repeat(50)]}}});
        let mut assembler = Assembler::new();
        let parts = fragments(&frame, 1, 16);
        assert!(parts.len() > 2);
        for part in &parts[..parts.len() - 1] {
            assert_eq!(assembler.accept(part), Assembled::Pending);
        }
        assert_eq!(
            assembler.accept(parts.last().unwrap()),
            Assembled::Frame {
                delivery_kind: "online".into(),
                frame: frame.clone()
            }
        );
        // 同一 ordinal 再来一遍：迟到重复，丢弃。
        assert_eq!(assembler.accept(&parts[0]), Assembled::Pending);
        let complete = json!({"wireVersion": 3, "kind": "complete", "deliveryKind": "online", "logicalFrameId": "f2", "logicalFrameOrdinal": 2, "topic": "sessions-index/w", "subscriptionId": "s", "frame": frame});
        assert!(matches!(
            assembler.accept(&complete),
            Assembled::Frame { .. }
        ));
    }

    #[test]
    fn faults_on_corruption_and_interleaving() {
        let frame =
            json!({"topic": "sessions-index/w", "subscriptionId": "s", "payload": "y".repeat(40)});
        let mut corrupt = fragments(&frame, 1, 16);
        corrupt[0]["checksum"]["value"] = json!("00000000");
        for part in corrupt.iter_mut() {
            part["checksum"]["value"] = json!("00000000");
        }
        let mut assembler = Assembler::new();
        let results: Vec<Assembled> = corrupt.iter().map(|p| assembler.accept(p)).collect();
        assert_eq!(
            results.last(),
            Some(&Assembled::Fault("proto.frameAssemblyChecksumMismatch"))
        );

        let mut assembler = Assembler::new();
        let first = fragments(&frame, 3, 16);
        let second = fragments(&frame, 4, 16);
        assert_eq!(assembler.accept(&first[0]), Assembled::Pending);
        assert_eq!(
            assembler.accept(&second[0]),
            Assembled::Fault("proto.frameAssemblyIncomplete")
        );
    }
}
