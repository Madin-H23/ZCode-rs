//! 闲时任务服务端的四个 JSON 接口（offPeakServerClient.ts）：额度快照、取号、批量状态、核销。
//! 无内建重试；排队与退避在调用方（service 的轮询）。
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::OnceCell;
use zcode_host_kit::log;

use super::gateway::Gateway;
use super::models::{Credentials, Unavailable, mock_enabled, resolve_credentials};
use crate::agent::service::uuid_v4;
use crate::support::http;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const TICKET_STATES: [&str; 6] = [
    "queued",
    "ready",
    "active",
    "expired",
    "settled",
    "not_found",
];

/// 类型化服务端错误：调用方按 bizCode 分流（3101 无资格 / 3103 取号超限）。
#[derive(Debug, Clone, PartialEq)]
pub struct ServerError {
    pub message: String,
    pub http_status: u16,
    pub biz_code: Option<i64>,
    pub next_take_at: Option<i64>,
}

/// 请求失败的分类（`classifyOffPeakCreateFailure` 按它分流）。
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    Server(ServerError),
    /// 响应不符合 schema（zod 失败）。
    InvalidResponse(String),
    Credentials(Unavailable),
    /// 网络、超时、origin 解析等。
    Other(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Server(error) => formatter.write_str(&error.message),
            ClientError::InvalidResponse(message) | ClientError::Other(message) => {
                formatter.write_str(message)
            }
            ClientError::Credentials(error) => write!(formatter, "{error}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TakeTicket {
    pub ticket_id: String,
    pub state: String,
    pub position: Option<i64>,
    pub next_poll_after_ms: Option<i64>,
    pub registered_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TicketStatus {
    pub ticket_id: String,
    pub state: String,
    pub position: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchStatus {
    pub next_poll_after_ms: Option<i64>,
    pub tickets: Vec<TicketStatus>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn invalid(what: &str) -> ClientError {
    ClientError::InvalidResponse(format!("invalid off-peak {what} response"))
}

fn state(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|state| TICKET_STATES.contains(state))
        .map(str::to_owned)
}

/// `position: z.number().int().nonnegative().nullish()`。
fn position(value: Option<&Value>) -> Result<Option<i64>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_i64().filter(|n| *n >= 0).map(Some).ok_or(()),
    }
}

/// 秒 → 毫秒（`next_poll_after` 非负数）。
fn poll_after(value: Option<&Value>) -> Result<Option<i64>, ()> {
    match value {
        None => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|seconds| *seconds >= 0.0)
            .map(|seconds| Some((seconds * 1000.0) as i64))
            .ok_or(()),
    }
}

pub struct ServerClient {
    vars: BTreeMap<String, String>,
    gateway: OnceCell<Option<Arc<Gateway>>>,
    gateway_port: Option<u16>,
    http: reqwest::Client,
}

impl ServerClient {
    /// `gateway_port` 只供测试覆盖 mock 网关端口（生产用 `ZCODE_OFFPEAK_MOCK_PORT` 或固定端口）。
    pub fn new(vars: BTreeMap<String, String>, gateway_port: Option<u16>) -> anyhow::Result<Self> {
        Ok(Self {
            vars,
            gateway: OnceCell::new(),
            gateway_port,
            http: http::client(REQUEST_TIMEOUT)?,
        })
    }

    pub fn credentials(&self) -> Result<Credentials, Unavailable> {
        resolve_credentials(&self.vars)
    }

    /// API origin：mock 模式懒启动进程内网关（多实例经端口占用复用同一份票据状态），否则 ZCode API。
    pub async fn origin(&self) -> Result<String, ClientError> {
        if !mock_enabled(&self.vars) {
            return http::zcode_api_origin(&self.vars)
                .map_err(|error| ClientError::Other(error.to_string()));
        }
        let gateway = self
            .gateway
            .get_or_init(|| async {
                match Gateway::start(&self.vars, self.gateway_port).await {
                    Ok(gateway) => Some(Arc::new(gateway)),
                    Err(error) => {
                        log::warn(format!("off-peak mock gateway failed to start: {error:#}"));
                        None
                    }
                }
            })
            .await;
        gateway
            .as_ref()
            .map(|gateway| gateway.origin())
            .ok_or_else(|| ClientError::Other("off-peak mock gateway failed to start".into()))
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ClientError> {
        let credentials = self.credentials().map_err(ClientError::Credentials)?;
        let origin = self.origin().await?;
        let mut request = self
            .http
            .request(method.clone(), format!("{origin}/api/v1/off-peak{path}"))
            .header("x-request-id", uuid_v4())
            .header("authorization", format!("Bearer {}", credentials.jwt))
            .header("x-coding-plan-api-key", &credentials.coding_plan_api_key);
        if let Some(body) = &body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.map_err(|error| {
            ClientError::Other(format!("off-peak {path} request failed: {error}"))
        })?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| ClientError::Other(format!("off-peak {path} read failed: {error}")))?;
        let json: Value = if text.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
        };
        if !status.is_success() {
            let code = json["code"].as_i64();
            let message = json["msg"].as_str().or_else(|| json["message"].as_str());
            log::warn(format!(
                "off-peak request rejected method={method} path={path} httpStatus={} bizCode={code:?} credentialKind={}",
                status.as_u16(),
                credentials.kind
            ));
            return Err(ClientError::Server(ServerError {
                message: format!(
                    "off-peak {path} failed: HTTP {}{}{}",
                    status.as_u16(),
                    code.filter(|code| *code != 0)
                        .map(|code| format!(" code={code}"))
                        .unwrap_or_default(),
                    message
                        .filter(|message| !message.is_empty())
                        .map(|message| format!(" {message}"))
                        .unwrap_or_default()
                ),
                http_status: status.as_u16(),
                biz_code: code,
                next_take_at: json["next_take_at"]
                    .as_i64()
                    .or_else(|| json["data"]["next_take_at"].as_i64()),
            }));
        }
        // 兼容裸体与 {code:0, data} 信封两种形态。
        if json.get("data").is_some() && json["code"] == 0 {
            return Ok(json["data"].clone());
        }
        Ok(json)
    }

    pub async fn take_number_availability(&self) -> Result<Value, ClientError> {
        let raw = self
            .request(reqwest::Method::GET, "/ticket/availability", None)
            .await?;
        let can = raw["can_take_number"]
            .as_bool()
            .ok_or_else(|| invalid("availability"))?;
        let next = match raw.get("next_take_at") {
            None => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| invalid("availability"))?,
            ),
        };
        if !can && next.is_none() {
            return Err(ClientError::Other(
                "off-peak availability missing next_take_at while unavailable".into(),
            ));
        }
        let mut out = serde_json::Map::new();
        out.insert("canTakeNumber".into(), json!(can));
        if let Some(next) = next {
            out.insert("nextTakeAt".into(), json!(next));
        }
        Ok(Value::Object(out))
    }

    pub async fn take_ticket(&self, task_id: &str) -> Result<TakeTicket, ClientError> {
        let raw = self
            .request(
                reqwest::Method::POST,
                "/ticket",
                Some(json!({"task_id": task_id})),
            )
            .await?;
        let ticket_id = raw["ticket_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| invalid("ticket"))?
            .to_owned();
        let state = state(&raw["state"]).ok_or_else(|| invalid("ticket"))?;
        let position = position(raw.get("position")).map_err(|()| invalid("ticket"))?;
        let next_poll_after_ms =
            poll_after(raw.get("next_poll_after")).map_err(|()| invalid("ticket"))?;
        log::info(format!(
            "off-peak take ticket ok task={task_id} ticket={ticket_id} state={state} position={}",
            position.map_or("-".to_owned(), |position| position.to_string())
        ));
        Ok(TakeTicket {
            ticket_id,
            state,
            position,
            next_poll_after_ms,
            registered_at: now_ms(),
        })
    }

    pub async fn batch_status(&self, ticket_ids: &[String]) -> Result<BatchStatus, ClientError> {
        if ticket_ids.is_empty() {
            return Ok(BatchStatus {
                next_poll_after_ms: None,
                tickets: Vec::new(),
            });
        }
        let limited = &ticket_ids[..ticket_ids.len().min(100)];
        if limited.len() < ticket_ids.len() {
            log::warn(format!(
                "off-peak batch status truncated {} -> 100",
                ticket_ids.len()
            ));
        }
        let raw = self
            .request(
                reqwest::Method::POST,
                "/ticket/status",
                Some(json!({"ticket_ids": limited})),
            )
            .await?;
        let next_poll_after_ms =
            poll_after(raw.get("next_poll_after")).map_err(|()| invalid("status"))?;
        let mut tickets = Vec::new();
        for entry in raw["tickets"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            tickets.push(TicketStatus {
                ticket_id: entry["ticket_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| invalid("status"))?
                    .to_owned(),
                state: state(&entry["state"]).ok_or_else(|| invalid("status"))?,
                position: position(entry.get("position")).map_err(|()| invalid("status"))?,
            });
        }
        Ok(BatchStatus {
            next_poll_after_ms,
            tickets,
        })
    }

    /// 幂等：重复上报 / 未知票一律 2xx。
    pub async fn settle(&self, ticket_id: &str) -> Result<(), ClientError> {
        let path = format!("/ticket/{}/settle", encode_uri_component(ticket_id));
        self.request(reqwest::Method::POST, &path, None).await?;
        log::info(format!("off-peak settle acked ticket={ticket_id}"));
        Ok(())
    }
}

/// JS `encodeURIComponent`。
fn encode_uri_component(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => (byte as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}
