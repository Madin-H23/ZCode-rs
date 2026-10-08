//! 闲时任务的进程内 mock 网关（offPeakMockGateway.ts），`ZCODE_OFFPEAK_MOCK=1` 时启用。
//!
//! 在 127.0.0.1 上模拟额度快照 / 取号 / 批量状态 / 核销 / messages。状态机与 Node 相同：
//! 取号 → queued；轮询时懒惰推进：超过 readyDelay → ready（readyDeadline = +readyTtl）；ready 超时
//! → expired；首个 message 准入 → active（activeDeadline = +active）；active 到期 → expired，
//! messages 回 400/3102；settle → settled（幂等）。端口固定（可用 `ZCODE_OFFPEAK_MOCK_PORT` 覆盖），
//! 已被另一个 Host 占用时复用它的票据状态。
//!
//! 准入后的 messages：Node 会代理到用户 Coding Plan 的真实端点；Rust Host 没有账号凭据（R4），
//! 与 Node 拿不到上游时一样回一段固定的 Anthropic 响应。
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zcode_host_kit::log;

const DEFAULT_PORT: u16 = 45_197;
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
struct Config {
    ready_delay_ms: i64,
    ready_ttl_ms: i64,
    active_ms: i64,
    queue_429_count: i64,
    quota_exhausted_count: i64,
    availability_blocked_ms: i64,
    retry_after_s: i64,
    next_poll_s: i64,
    /// `capture` / `invalid-ticket`；`foreground-subagents` 的脚本化响应未移植（只用于 Node E2E）。
    scenario: Option<String>,
}

impl Config {
    fn from_env(vars: &BTreeMap<String, String>) -> Self {
        let num = |key: &str, default: i64| {
            vars.get(key)
                .filter(|raw| !raw.is_empty())
                .and_then(|raw| raw.trim().parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map_or(default, |value| value as i64)
        };
        Self {
            ready_delay_ms: num("ZCODE_OFFPEAK_MOCK_READY_DELAY_MS", 15_000),
            ready_ttl_ms: num("ZCODE_OFFPEAK_MOCK_READY_TTL_MS", 5 * 60_000),
            active_ms: num("ZCODE_OFFPEAK_MOCK_ACTIVE_MS", 3 * 60 * 60_000),
            queue_429_count: num("ZCODE_OFFPEAK_MOCK_QUEUE_429_COUNT", 0),
            quota_exhausted_count: num("ZCODE_OFFPEAK_MOCK_QUOTA_COUNT", 0),
            availability_blocked_ms: num("ZCODE_OFFPEAK_MOCK_AVAILABILITY_BLOCK_MS", 0),
            retry_after_s: num("ZCODE_OFFPEAK_MOCK_RETRY_AFTER_S", 5),
            next_poll_s: num("ZCODE_OFFPEAK_MOCK_NEXT_POLL_S", 5),
            scenario: vars
                .get("ZCODE_OFFPEAK_MOCK_SCENARIO")
                .filter(|value| {
                    matches!(
                        value.as_str(),
                        "foreground-subagents" | "capture" | "invalid-ticket"
                    )
                })
                .cloned(),
        }
    }
}

#[derive(Debug, Clone)]
struct Ticket {
    ticket_id: String,
    task_id: String,
    state: &'static str,
    taken_at: i64,
    ready_deadline: Option<i64>,
    active_deadline: Option<i64>,
    queue_429_remaining: i64,
    seq: u64,
}

#[derive(Default)]
struct State {
    tickets: HashMap<String, Ticket>,
    latest_by_task: HashMap<String, String>,
    quota_remaining: i64,
    availability_blocked_until: i64,
    seq: u64,
    captured: Vec<Value>,
    invalid_ticket_requests: u64,
}

struct Shared {
    config: Config,
    state: Mutex<State>,
}

pub struct Gateway {
    origin: String,
    task: Option<tokio::task::AbortHandle>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

impl Gateway {
    /// 绑定端口：`port` 覆盖（测试用 0 取临时端口），否则 `ZCODE_OFFPEAK_MOCK_PORT`，再否则固定端口。
    pub async fn start(vars: &BTreeMap<String, String>, port: Option<u16>) -> anyhow::Result<Self> {
        let port = port.unwrap_or_else(|| {
            vars.get("ZCODE_OFFPEAK_MOCK_PORT")
                .and_then(|raw| raw.trim().parse::<u16>().ok())
                .filter(|port| *port > 0)
                .unwrap_or(DEFAULT_PORT)
        });
        let listener = match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                let origin = format!("http://127.0.0.1:{port}");
                log::info(format!(
                    "off-peak mock gateway reusing existing instance at {origin}"
                ));
                return Ok(Self { origin, task: None });
            }
            Err(error) => return Err(error.into()),
        };
        let origin = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let config = Config::from_env(vars);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                quota_remaining: config.quota_exhausted_count,
                availability_blocked_until: now_ms() + config.availability_blocked_ms,
                ..State::default()
            }),
            config,
        });
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve(stream, shared).await {
                        log::debug(format!("off-peak mock gateway connection failed: {error}"));
                    }
                });
            }
        });
        log::info(format!("off-peak mock gateway listening at {origin}"));
        Ok(Self {
            origin,
            task: Some(task.abort_handle()),
        })
    }

    pub fn origin(&self) -> String {
        self.origin.clone()
    }
}

/// 一个最小的 HTTP/1.1 请求。
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// 读一个请求（只支持 Content-Length 请求体；回应后关闭连接）。
pub(crate) async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_REQUEST_BYTES {
            return Ok(None);
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

pub(crate) async fn write_json(
    stream: &mut TcpStream,
    status: u16,
    body: &Value,
    extra: &[(&str, String)],
) -> std::io::Result<()> {
    let text = body.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        text.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(text.as_bytes()).await?;
    stream.shutdown().await
}

fn advance(ticket: &mut Ticket, config: &Config, now: i64) {
    if ticket.state == "queued" && now - ticket.taken_at >= config.ready_delay_ms {
        ticket.state = "ready";
        ticket.ready_deadline = Some(now + config.ready_ttl_ms);
    }
    if ticket.state == "ready" && ticket.ready_deadline.is_some_and(|deadline| now > deadline) {
        ticket.state = "expired";
    }
    if ticket.state == "active"
        && ticket
            .active_deadline
            .is_some_and(|deadline| now > deadline)
    {
        ticket.state = "expired";
    }
}

/// 排队位次 = 前面还有多少张未晋级的活票 + 1。
fn position_of(state: &State, ticket: &Ticket) -> i64 {
    let ahead = state
        .tickets
        .values()
        .filter(|other| other.state == "queued" && other.seq < ticket.seq)
        .count();
    ahead as i64 + 1
}

fn body_json(body: &[u8]) -> Value {
    if body.is_empty() {
        return json!({});
    }
    serde_json::from_slice(body).unwrap_or_else(|_| json!({}))
}

async fn serve(mut stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let (status, body, extra) = route(&shared, &request);
    write_json(&mut stream, status, &body, &extra).await
}

type Response = (u16, Value, Vec<(&'static str, String)>);

fn route(shared: &Shared, request: &Request) -> Response {
    let config = &shared.config;
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = now_ms();
    let path = request.path.as_str();
    if request.method == "GET" && path == "/__e2e/off-peak/requests" && config.scenario.is_some() {
        return (
            200,
            json!({"invalidTicketRequests": state.invalid_ticket_requests, "requests": state.captured}),
            vec![],
        );
    }
    if request.method == "GET" && path == "/api/v1/off-peak/ticket/availability" {
        let blocked = now < state.availability_blocked_until;
        let mut body = json!({"can_take_number": !blocked});
        if blocked {
            body["next_take_at"] = json!(state.availability_blocked_until);
        }
        return (200, body, vec![]);
    }
    if request.method != "POST" {
        return (405, json!({"msg": "method not allowed"}), vec![]);
    }
    match path {
        "/api/v1/off-peak/ticket" => take(config, &mut state, &body_json(&request.body), now),
        "/api/v1/off-peak/ticket/status" => {
            status(config, &mut state, &body_json(&request.body), now)
        }
        "/api/v1/off-peak/anthropic/v1/messages" => messages(config, &mut state, request, now),
        _ => match path
            .strip_prefix("/api/v1/off-peak/ticket/")
            .and_then(|rest| rest.strip_suffix("/settle"))
            .filter(|id| !id.is_empty() && !id.contains('/'))
        {
            Some(encoded) => {
                let ticket_id = percent_decode(encoded);
                if let Some(ticket) = state.tickets.get_mut(&ticket_id) {
                    ticket.state = "settled";
                }
                (
                    200,
                    json!({"ticket_id": ticket_id, "state": "settled", "settled_at": now}),
                    vec![],
                )
            }
            None => (
                404,
                json!({"msg": format!("no mock route for {path}")}),
                vec![],
            ),
        },
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn take(config: &Config, state: &mut State, body: &Value, now: i64) -> Response {
    let Some(task_id) = body["task_id"].as_str().filter(|id| !id.is_empty()) else {
        return (
            400,
            json!({"code": 3000, "msg": "task_id required"}),
            vec![],
        );
    };
    if now < state.availability_blocked_until {
        let until = state.availability_blocked_until;
        return (
            429,
            json!({"code": 3103, "msg": "free tier limit reached", "data": {"next_take_at": until}}),
            vec![],
        );
    }
    if state.quota_remaining > 0 {
        state.quota_remaining -= 1;
        return (
            429,
            json!({"code": 3103, "msg": "free tier limit reached", "data": {"next_take_at": now + 60_000}}),
            vec![],
        );
    }
    // 同 task_id 重新取号：旧票直接作废。
    if let Some(previous) = state.latest_by_task.get(task_id).cloned()
        && let Some(old) = state.tickets.get_mut(&previous)
        && matches!(old.state, "queued" | "ready" | "active")
    {
        old.state = "expired";
    }
    state.seq += 1;
    let mut ticket = Ticket {
        ticket_id: format!("mock-ticket-{}", state.seq),
        task_id: task_id.to_owned(),
        state: "queued",
        taken_at: now,
        ready_deadline: None,
        active_deadline: None,
        queue_429_remaining: config.queue_429_count,
        seq: state.seq,
    };
    advance(&mut ticket, config, now);
    let position = (ticket.state == "queued").then(|| position_of(state, &ticket));
    log::info(format!(
        "mock gateway take task={task_id} ticket={} state={}",
        ticket.ticket_id, ticket.state
    ));
    let mut body = json!({
        "ticket_id": ticket.ticket_id,
        "task_id": task_id,
        "state": ticket.state,
        "accepted": true,
        "position": position,
        "next_poll_after": config.next_poll_s,
        "queued_at": now,
    });
    if let Some(deadline) = ticket.ready_deadline {
        body["ready_deadline"] = json!(deadline);
    }
    state
        .latest_by_task
        .insert(task_id.to_owned(), ticket.ticket_id.clone());
    state.tickets.insert(ticket.ticket_id.clone(), ticket);
    (200, body, vec![])
}

fn status(config: &Config, state: &mut State, body: &Value, now: i64) -> Response {
    let ids: Vec<String> = body["ticket_ids"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .take(100)
                .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut entries = Vec::new();
    for id in ids {
        let Some(mut ticket) = state.tickets.get(&id).cloned() else {
            entries.push(json!({"ticket_id": id, "state": "not_found"}));
            continue;
        };
        advance(&mut ticket, config, now);
        let position = (ticket.state == "queued").then(|| position_of(state, &ticket));
        let mut entry = json!({
            "ticket_id": ticket.ticket_id,
            "task_id": ticket.task_id,
            "state": ticket.state,
            "position": position,
        });
        if let Some(deadline) = ticket.active_deadline {
            entry["active_deadline"] = json!(deadline);
        }
        entries.push(entry);
        state.tickets.insert(id, ticket);
    }
    (
        200,
        json!({"next_poll_after": config.next_poll_s, "tickets": entries}),
        vec![],
    )
}

fn messages(config: &Config, state: &mut State, request: &Request, now: i64) -> Response {
    let ticket_id = request
        .headers
        .get("x-off-peak-ticket-id")
        .cloned()
        .unwrap_or_default();
    let wrong = (
        400,
        json!({"code": 3102, "msg": "wrong off-peak ticket"}),
        vec![],
    );
    let Some(mut ticket) = state.tickets.get(&ticket_id).cloned() else {
        return wrong;
    };
    advance(&mut ticket, config, now);
    let result = (|| {
        if matches!(ticket.state, "expired" | "settled") {
            return Some(wrong.clone());
        }
        if config.scenario.as_deref() == Some("invalid-ticket") {
            state.invalid_ticket_requests += 1;
            return Some(wrong.clone());
        }
        let retry = vec![("retry-after", config.retry_after_s.to_string())];
        if ticket.state == "queued" {
            return Some((429, json!({"code": 3105, "msg": "not ready"}), retry));
        }
        if ticket.queue_429_remaining > 0 {
            ticket.queue_429_remaining -= 1;
            return Some((
                429,
                json!({"code": 3105, "msg": "model concurrency saturated (mock)"}),
                retry,
            ));
        }
        if ticket.state == "ready" {
            ticket.state = "active";
            ticket.active_deadline = Some(now + config.active_ms);
            log::info(format!(
                "mock gateway admitted ticket={ticket_id} activeDeadline={}",
                now + config.active_ms
            ));
        }
        None
    })();
    state.tickets.insert(ticket_id.clone(), ticket);
    if let Some(response) = result {
        return response;
    }
    if config.scenario.as_deref() == Some("capture") {
        let request_json = body_json(&request.body);
        let header = |name: &str| request.headers.get(name).cloned();
        let mut captured = json!({
            "hasApiKey": header("x-api-key").is_some(),
            "hasAuthorization": header("authorization").is_some(),
            "hasCodingPlanApiKey": header("x-coding-plan-api-key").is_some(),
            "requestJson": request_json,
            "stage": "unknown",
            "ticketId": ticket_id,
        });
        if let Some(max_tokens) = request_json["max_tokens"].as_f64() {
            captured["maxTokens"] = json!(max_tokens);
        }
        if let Some(model) = request_json["model"].as_str() {
            captured["model"] = json!(model);
        }
        state.captured.push(captured);
    }
    // 离线：回一段固定的非流式 Anthropic 响应，agent loop 立即拿到 stop 收尾。
    (
        200,
        json!({
            "id": "msg_offpeak_mock",
            "type": "message",
            "role": "assistant",
            "model": "offpeak-mock",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "Off-peak mock run completed."}],
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }),
        vec![],
    )
}
