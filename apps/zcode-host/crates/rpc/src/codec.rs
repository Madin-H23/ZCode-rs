//! 值编码与消息头（`packages/rpc/src/serialization.ts`、`channels.shared.ts`）。
//!
//! 每条消息 = serialize(header) + serialize(body)。int32 走 VQL Int，其余数字、`null`、
//! 布尔、对象走 Object（JSON 文本）；`Uint8Array` 走 Buffer，对象内嵌的 `Uint8Array`
//! 以 `{"__zcode_rpc_nested_uint8array_v1":true,"base64":…}` 写入 JSON。
use anyhow::{Result, bail};
use serde_json::{Map, Value};

const UNDEFINED: u8 = 0;
const STRING: u8 = 1;
const BUFFER: u8 = 2;
const VS_BUFFER: u8 = 3;
const ARRAY: u8 = 4;
const OBJECT: u8 = 5;
const INT: u8 = 6;

const NESTED_BYTES_MARKER: &str = "__zcode_rpc_nested_uint8array_v1";
const NESTED_BYTES_BASE64: &str = "base64";

pub const REQUEST_PROMISE: i32 = 100;
pub const REQUEST_PROMISE_CANCEL: i32 = 101;
pub const REQUEST_EVENT_LISTEN: i32 = 102;
pub const REQUEST_EVENT_DISPOSE: i32 = 103;
pub const RESPONSE_INITIALIZE: i32 = 200;
pub const RESPONSE_PROMISE_SUCCESS: i32 = 201;
pub const RESPONSE_PROMISE_ERROR: i32 = 202;
pub const RESPONSE_PROMISE_ERROR_OBJ: i32 = 203;
pub const RESPONSE_EVENT_FIRE: i32 = 204;

/// 反序列化结果。`Undefined` 与 JSON `null` 在 wire 上不同，保留区分。
#[derive(Debug, Clone, PartialEq)]
pub enum RpcValue {
    Undefined,
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<RpcValue>),
    Json(Value),
    Int(i32),
}

impl RpcValue {
    /// 转成 JSON 供 channel 读取参数：Undefined 视为 null，字节串保持嵌套字节标记的形态。
    pub fn into_json(self) -> Value {
        match self {
            RpcValue::Undefined => Value::Null,
            RpcValue::String(text) => Value::String(text),
            RpcValue::Bytes(bytes) => nested_bytes(&bytes),
            RpcValue::Array(items) => {
                Value::Array(items.into_iter().map(Self::into_json).collect())
            }
            RpcValue::Json(value) => value,
            RpcValue::Int(value) => Value::from(value),
        }
    }

    pub fn as_int(&self) -> Option<i32> {
        match self {
            RpcValue::Int(value) => Some(*value),
            RpcValue::Json(value) => value.as_i64().and_then(|v| i32::try_from(v).ok()),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            RpcValue::String(text) => Some(text),
            RpcValue::Json(Value::String(text)) => Some(text),
            _ => None,
        }
    }
}

fn nested_bytes(bytes: &[u8]) -> Value {
    let mut object = Map::new();
    object.insert(NESTED_BYTES_MARKER.into(), Value::Bool(true));
    object.insert(
        NESTED_BYTES_BASE64.into(),
        Value::String(base64_encode(bytes)),
    );
    Value::Object(object)
}

/// TS 写入用无符号右移（`>>>`），负数按 32 位补码写 5 字节。
pub fn write_vql(out: &mut Vec<u8>, value: i32) {
    let mut value = value as u32;
    if value == 0 {
        out.push(0);
        return;
    }
    while value != 0 {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value > 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
}

fn write_length(out: &mut Vec<u8>, length: usize) {
    // TS 端长度同样经 writeInt32VQL；超出 int32 的载荷在 TS 侧也无法表示。
    write_vql(out, i32::try_from(length).unwrap_or(i32::MAX));
}

pub fn write_undefined(out: &mut Vec<u8>) {
    out.push(UNDEFINED);
}

pub fn write_int(out: &mut Vec<u8>, value: i32) {
    out.push(INT);
    write_vql(out, value);
}

pub fn write_string(out: &mut Vec<u8>, text: &str) {
    out.push(STRING);
    write_length(out, text.len());
    out.extend_from_slice(text.as_bytes());
}

pub fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(BUFFER);
    write_length(out, bytes.len());
    out.extend_from_slice(bytes);
}

/// Object 标签直接写入一段已是 JSON 的字节，不重新编码。
pub fn write_json_bytes(out: &mut Vec<u8>, json: &[u8]) {
    out.push(OBJECT);
    write_length(out, json.len());
    out.extend_from_slice(json);
}

fn int32_of(number: &serde_json::Number) -> Option<i32> {
    if let Some(value) = number.as_i64() {
        return i32::try_from(value).ok();
    }
    // JS 的 `(data | 0) === data`：1.0 这类整数值同样走 Int。
    let value = number.as_f64()?;
    (value.fract() == 0.0 && value >= i32::MIN as f64 && value <= i32::MAX as f64)
        .then_some(value as i32)
}

/// 与 TS `serialize` 相同的分派：数组逐项、字符串、int32，其余 JSON。
pub fn write_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push(ARRAY);
            write_length(out, items.len());
            for item in items {
                write_value(out, item);
            }
        }
        Value::Number(number) if int32_of(number).is_some() => {
            write_int(out, int32_of(number).unwrap_or_default())
        }
        other => write_json_bytes(out, other.to_string().as_bytes()),
    }
}

/// 已是 JSON 文本的值（CLI 的 result、V4 帧 params）直接写出：对象原样走 Object 标签，
/// 不建对象树、不重新编码。数组在 TS 侧会走 Array 标签，这里保持 Object，Renderer 解出的值相同。
pub fn write_raw_json(out: &mut Vec<u8>, raw: &str) -> Result<()> {
    let trimmed = raw.trim();
    match trimmed.as_bytes().first() {
        Some(b'"') => write_string(out, &serde_json::from_str::<String>(trimmed)?),
        Some(b'-' | b'0'..=b'9') => write_value(out, &serde_json::from_str::<Value>(trimmed)?),
        Some(_) => write_json_bytes(out, trimmed.as_bytes()),
        None => bail!("empty JSON value"),
    }
    Ok(())
}

pub struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .filter(|end| *end <= self.data.len());
        let Some(end) = end else {
            bail!("rpc message truncated");
        };
        let slice = &self.data[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    /// 与 TS readIntVQL 相同：`value |= (byte & 0x7f) << n`，按 int32 截断。
    fn vql(&mut self) -> Result<i32> {
        let mut value: u32 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.take(1)?[0];
            value |= ((byte & 0x7f) as u32).wrapping_shl(shift);
            if byte & 0x80 == 0 {
                return Ok(value as i32);
            }
            shift += 7;
            if shift > 35 {
                bail!("VQL too long");
            }
        }
    }

    fn length(&mut self) -> Result<usize> {
        usize::try_from(self.vql()?).map_err(|_| anyhow::anyhow!("negative length"))
    }

    pub fn value(&mut self) -> Result<RpcValue> {
        Ok(match self.take(1)?[0] {
            UNDEFINED => RpcValue::Undefined,
            STRING => {
                let length = self.length()?;
                RpcValue::String(String::from_utf8_lossy(self.take(length)?).into_owned())
            }
            BUFFER | VS_BUFFER => {
                let length = self.length()?;
                RpcValue::Bytes(self.take(length)?.to_vec())
            }
            ARRAY => {
                let length = self.length()?;
                let mut items = Vec::with_capacity(length.min(1024));
                for _ in 0..length {
                    items.push(self.value()?);
                }
                RpcValue::Array(items)
            }
            OBJECT => {
                let length = self.length()?;
                RpcValue::Json(serde_json::from_slice(self.take(length)?)?)
            }
            INT => RpcValue::Int(self.vql()?),
            tag => bail!("unknown rpc tag {tag}"),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.offset >= self.data.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Promise,
    PromiseCancel,
    EventListen,
    EventDispose,
}

/// 一条请求：header `[type, id, channel?, name?]` 加 body（call 为参数数组，listen 为单个参数）。
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub kind: RequestKind,
    pub id: i32,
    pub channel: String,
    pub name: String,
    pub body: RpcValue,
}

pub fn read_request(message: &[u8]) -> Result<Request> {
    let mut reader = Reader::new(message);
    let RpcValue::Array(items) = reader.value()? else {
        bail!("rpc header is not an array");
    };
    let kind = match items.first().and_then(RpcValue::as_int) {
        Some(REQUEST_PROMISE) => RequestKind::Promise,
        Some(REQUEST_PROMISE_CANCEL) => RequestKind::PromiseCancel,
        Some(REQUEST_EVENT_LISTEN) => RequestKind::EventListen,
        Some(REQUEST_EVENT_DISPOSE) => RequestKind::EventDispose,
        other => bail!("unknown rpc request type {other:?}"),
    };
    let Some(id) = items.get(1).and_then(RpcValue::as_int) else {
        bail!("rpc request without id");
    };
    let text = |index: usize| {
        items
            .get(index)
            .and_then(RpcValue::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let body = if reader.is_empty() {
        RpcValue::Undefined
    } else {
        reader.value()?
    };
    Ok(Request {
        kind,
        id,
        channel: text(2),
        name: text(3),
        body,
    })
}

fn response_header(out: &mut Vec<u8>, kind: i32, id: i32) {
    out.push(ARRAY);
    write_vql(out, 2);
    write_int(out, kind);
    write_int(out, id);
}

/// `[200]` + undefined body。
pub fn initialize() -> Vec<u8> {
    let mut out = Vec::with_capacity(6);
    out.push(ARRAY);
    write_vql(&mut out, 1);
    write_int(&mut out, RESPONSE_INITIALIZE);
    write_undefined(&mut out);
    out
}

/// kind 为 PromiseSuccess 或 EventFire。
pub fn response_value(kind: i32, id: i32, value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    response_header(&mut out, kind, id);
    write_value(&mut out, value);
    out
}

/// kind 为 PromiseSuccess 或 EventFire，body 为 undefined。
pub fn response_undefined(kind: i32, id: i32) -> Vec<u8> {
    let mut out = Vec::new();
    response_header(&mut out, kind, id);
    write_undefined(&mut out);
    out
}

/// kind 为 PromiseSuccess 或 EventFire，body 为顶层二进制。
pub fn response_bytes(kind: i32, id: i32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 16);
    response_header(&mut out, kind, id);
    write_bytes(&mut out, bytes);
    out
}

pub fn success_value(id: i32, value: &Value) -> Vec<u8> {
    response_value(RESPONSE_PROMISE_SUCCESS, id, value)
}

pub fn success_undefined(id: i32) -> Vec<u8> {
    response_undefined(RESPONSE_PROMISE_SUCCESS, id)
}

/// kind 为 PromiseSuccess 或 EventFire，data 为原始 JSON。
pub fn raw_response(kind: i32, id: i32, raw: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len() + 16);
    response_header(&mut out, kind, id);
    write_raw_json(&mut out, raw)?;
    Ok(out)
}

/// 与 `ChannelServer` 对 `Error` 实例的拒绝载荷相同：`{message, name, stack?}` 加透传键。
/// `ChannelClient` 会按 9 个透传键把 `code` 等字段还原到 Error 上。
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub name: String,
    pub message: String,
    /// 透传键 `code`：Host 自身的错误是字符串，CLI 业务错误是 JSON-RPC 数字码。
    pub code: Option<Box<Value>>,
    pub data: Option<Box<Value>>,
    /// 其余透传键（`kind`、`status`、`retryAfterMs`、`detail`、`details`、`taskId`、`traceId`）。
    pub fields: Option<Box<Map<String, Value>>>,
}

/// `ChannelServer` / `ChannelClient` 的 9 个透传键中，除 `code`、`data` 之外的部分。
const PASSTHROUGH_FIELDS: [&str; 7] = [
    "kind",
    "status",
    "retryAfterMs",
    "detail",
    "details",
    "taskId",
    "traceId",
];

impl RpcError {
    pub fn new(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
            code: None,
            data: None,
            fields: None,
        }
    }

    /// 设置一个透传键；不是透传键的名字会被忽略（ChannelClient 不会还原它）。
    pub fn with_field(mut self, key: &str, value: impl Into<Value>) -> Self {
        if PASSTHROUGH_FIELDS.contains(&key) {
            self.fields
                .get_or_insert_with(Default::default)
                .insert(key.to_owned(), value.into());
        }
        self
    }

    pub fn field(&self, key: &str) -> Option<&Value> {
        self.fields.as_deref().and_then(|fields| fields.get(key))
    }

    pub fn with_code(mut self, code: impl Into<Value>) -> Self {
        self.code = Some(Box::new(code.into()));
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(Box::new(data));
        self
    }

    pub fn code_str(&self) -> Option<&str> {
        self.code.as_deref().and_then(Value::as_str)
    }

    pub fn code_i64(&self) -> Option<i64> {
        self.code.as_deref().and_then(Value::as_i64)
    }

    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("message".into(), Value::String(self.message.clone()));
        object.insert("name".into(), Value::String(self.name.clone()));
        if let Some(code) = &self.code {
            object.insert("code".into(), Value::clone(code));
        }
        if let Some(data) = &self.data {
            object.insert("data".into(), Value::clone(data));
        }
        if let Some(fields) = &self.fields {
            for (key, value) in fields.iter() {
                object.insert(key.clone(), value.clone());
            }
        }
        Value::Object(object)
    }
}

pub fn promise_error(id: i32, error: &RpcError) -> Vec<u8> {
    let mut out = Vec::new();
    response_header(&mut out, RESPONSE_PROMISE_ERROR, id);
    write_value(&mut out, &error.to_json());
    out
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 与 Node `Buffer.toString("base64")` 相同的标准 base64（带填充）。
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(BASE64_ALPHABET[((triple >> (18 - 6 * index)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ---- 客户端方向（`channelClient.ts`）：请求编码与响应解析 ----

/// 数组标签与长度；元素由调用方逐个写入。
pub fn write_array_header(out: &mut Vec<u8>, length: usize) {
    out.push(ARRAY);
    write_length(out, length);
}

/// 按收到时的标签原样写回一个值（转发用）。`Json` 一律走 Object 标签，与解码前相同；
/// 二进制统一写 Buffer 标签（TS 侧解出 `Uint8Array`）。
pub fn write_rpc_value(out: &mut Vec<u8>, value: &RpcValue) {
    match value {
        RpcValue::Undefined => write_undefined(out),
        RpcValue::String(text) => write_string(out, text),
        RpcValue::Bytes(bytes) => write_bytes(out, bytes),
        RpcValue::Array(items) => {
            write_array_header(out, items.len());
            for item in items {
                write_rpc_value(out, item);
            }
        }
        RpcValue::Json(json) => write_json_bytes(out, json.to_string().as_bytes()),
        RpcValue::Int(number) => write_int(out, *number),
    }
}

/// `[type, id, channel, name]` + body：call（100）的 body 是参数数组，listen（102）是单个参数。
pub fn encode_request(kind: i32, id: i32, channel: &str, name: &str, body: &RpcValue) -> Vec<u8> {
    let mut out = Vec::new();
    write_array_header(&mut out, 4);
    write_int(&mut out, kind);
    write_int(&mut out, id);
    write_string(&mut out, channel);
    write_string(&mut out, name);
    write_rpc_value(&mut out, body);
    out
}

/// `[101, id]` 取消调用或 `[103, id]` 退订，body 为 undefined。
pub fn encode_cancel(kind: i32, id: i32) -> Vec<u8> {
    let mut out = Vec::new();
    response_header(&mut out, kind, id);
    write_undefined(&mut out);
    out
}

/// 一条响应：`[200]`、`[201|202|203|204, id]` 加 body。
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub kind: i32,
    pub id: Option<i32>,
    pub body: RpcValue,
}

pub fn read_response(message: &[u8]) -> Result<Response> {
    let mut reader = Reader::new(message);
    let RpcValue::Array(items) = reader.value()? else {
        bail!("rpc response header is not an array");
    };
    let Some(kind) = items.first().and_then(RpcValue::as_int) else {
        bail!("rpc response without type");
    };
    let id = items.get(1).and_then(RpcValue::as_int);
    let body = if reader.is_empty() {
        RpcValue::Undefined
    } else {
        reader.value()?
    };
    Ok(Response { kind, id, body })
}

impl RpcError {
    /// `[202]` 载荷还原（`ChannelClient` 的 PromiseError 分支）：message、name 与透传键。
    pub fn from_payload(payload: &Value) -> Self {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or_default();
        let mut error = RpcError::new(
            match text("name") {
                "" => "Error",
                name => name,
            },
            text("message"),
        );
        if let Some(code) = payload.get("code").filter(|code| !code.is_null()) {
            error = error.with_code(code.clone());
        }
        if let Some(data) = payload.get("data").filter(|data| !data.is_null()) {
            error = error.with_data(data.clone());
        }
        for key in PASSTHROUGH_FIELDS {
            if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
                error = error.with_field(key, value.clone());
            }
        }
        error
    }
}
