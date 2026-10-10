//! 合并前在两个测试文件里逐字复制的辅助代码，现在共用一份。
use serde_json::Value;

/// 与 packages/rpc 的真实编码对拍的夹具（由 `tests/fixtures/gen.ts` 生成）。
pub(super) fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/rpc-serialization.json")).unwrap()
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
