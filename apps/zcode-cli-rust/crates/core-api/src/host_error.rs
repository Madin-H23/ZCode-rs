//! Host 对反向请求的错误应答（docs/specs/rust-u0-foundation.md §4.2，缺陷 B5）。
use serde_json::Value;
use std::fmt;

/// JSON-RPC `error` 对象：`{code, message, data?}`。
///
/// 以前 stdio 把 Host 的 error 应答当成 `result: null`，等待方只能看到空结果，Host 给出的
/// 错误原因随之丢失；现在应答以 `Result<Value, HostError>` 交给等待方。
#[derive(Clone, Debug, PartialEq)]
pub struct HostError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl HostError {
    /// 解析 Host 发来的 `error` 字段；字段缺失时使用 JSON-RPC 的内部错误码与通用说明。
    pub fn from_error_object(error: &Value) -> Self {
        Self {
            code: error["code"].as_i64().unwrap_or(-32603),
            message: error["message"]
                .as_str()
                .unwrap_or("Host request failed")
                .to_owned(),
            data: error.get("data").cloned(),
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "host error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for HostError {}
