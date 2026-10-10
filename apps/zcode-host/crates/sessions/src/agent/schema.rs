//! 发给 Renderer 的 CLI 事实的严格校验（docs/specs/rust-host-agent.md §2.9）。
//!
//! Node 用 strict zod schema 校验 `v4/telemetry/local-ttft`、`v4/telemetry/event`、
//! `v4/cua/permission-observation`，未知字段整条丢弃：CLI 新增字段不能未经审计穿透到 Renderer。
//! 这里用声明式字段表逐字段对应 TS schema（`localTtft.ts`、`zcode-protocol-v4/telemetry.ts`、
//! `cuaPermission.ts`），单测用 TS 生成的样本对照判定结果。
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy)]
pub enum Ty {
    /// 字符串长度按 UTF-16 码元计（与 JS 相同）。
    Str {
        min: usize,
        max: Option<usize>,
    },
    /// `z.number().int()`，可选非负。
    Int {
        nonnegative: bool,
        positive: bool,
    },
    /// `z.number()`，可选非负。
    Num {
        nonnegative: bool,
    },
    Bool,
    Enum(&'static [&'static str]),
    NumLiteral(f64),
    StrLiteral(&'static str),
    Uuid,
    /// 16 位小写十六进制（commandHash）。
    Hex16,
    ProviderHostname,
    Object(&'static [Field]),
    Array {
        item: &'static Ty,
        max: usize,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub name: &'static str,
    pub ty: Ty,
    pub required: bool,
}

const fn req(name: &'static str, ty: Ty) -> Field {
    Field {
        name,
        ty,
        required: true,
    }
}

const fn opt(name: &'static str, ty: Ty) -> Field {
    Field {
        name,
        ty,
        required: false,
    }
}

const fn s(min: usize, max: Option<usize>) -> Ty {
    Ty::Str { min, max }
}

const STR: Ty = s(0, None);
const NONEMPTY: Ty = s(1, None);
const IDENT: Ty = s(1, Some(128));
const INT: Ty = Ty::Int {
    nonnegative: false,
    positive: false,
};
const UINT: Ty = Ty::Int {
    nonnegative: true,
    positive: false,
};
const PINT: Ty = Ty::Int {
    nonnegative: false,
    positive: true,
};
const NUM: Ty = Ty::Num { nonnegative: false };
const UNUM: Ty = Ty::Num { nonnegative: true };

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn as_integer(value: &Value) -> Option<f64> {
    let number = value.as_f64()?;
    (number.is_finite() && number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0)
        .then_some(number)
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
        && matches!(bytes[14], b'1'..=b'8')
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
}

fn is_provider_hostname(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if let Some(inner) = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        && !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
    {
        return true;
    }
    !value.chars().any(|c| {
        c.is_whitespace() || c == '\u{feff}' || matches!(c, '/' | ':' | '?' | '#' | '@' | '[' | ']')
    })
}

pub fn check(value: &Value, ty: &Ty) -> bool {
    match ty {
        Ty::Str { min, max } => value.as_str().is_some_and(|text| {
            let len = utf16_len(text);
            len >= *min && max.is_none_or(|max| len <= max)
        }),
        Ty::Int {
            nonnegative,
            positive,
        } => {
            as_integer(value).is_some_and(|n| (!nonnegative || n >= 0.0) && (!positive || n > 0.0))
        }
        Ty::Num { nonnegative } => value
            .as_f64()
            .is_some_and(|n| n.is_finite() && (!nonnegative || n >= 0.0)),
        Ty::Bool => value.is_boolean(),
        Ty::Enum(options) => value.as_str().is_some_and(|text| options.contains(&text)),
        Ty::NumLiteral(expected) => value.as_f64() == Some(*expected),
        Ty::StrLiteral(expected) => value.as_str() == Some(*expected),
        Ty::Uuid => value.as_str().is_some_and(is_uuid),
        Ty::Hex16 => value.as_str().is_some_and(|text| {
            text.len() == 16 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }),
        Ty::ProviderHostname => value.as_str().is_some_and(is_provider_hostname),
        Ty::Object(fields) => value
            .as_object()
            .is_some_and(|object| check_object(object, fields)),
        Ty::Array { item, max } => value
            .as_array()
            .is_some_and(|items| items.len() <= *max && items.iter().all(|v| check(v, item))),
    }
}

/// strict object：未知键、缺必填键、类型不符都判不合法。
pub fn check_object(object: &Map<String, Value>, fields: &[Field]) -> bool {
    if object
        .keys()
        .any(|key| !fields.iter().any(|field| field.name == key))
    {
        return false;
    }
    fields.iter().all(|field| match object.get(field.name) {
        None => !field.required,
        Some(value) => check(value, &field.ty),
    })
}

// ── localTtftFactsSchema ──

const TTFT_DETAIL: &[Field] = &[
    req("id", IDENT),
    req(
        "stage",
        Ty::Enum(&[
            "context",
            "hooks",
            "persistence",
            "compaction",
            "mcp",
            "tools",
            "request_assembly",
            "attempt",
            "retry_wait",
            "user_confirmation",
        ]),
    ),
    req("start", UNUM),
    opt("end", UNUM),
    opt(
        "outcome",
        Ty::Enum(&["completed", "failed", "cancelled", "first_output"]),
    ),
    opt("requestId", IDENT),
    opt("logicalCallId", IDENT),
    opt("role", Ty::Enum(&["response", "preparation"])),
    req("source", Ty::Enum(&["renderer", "cli"])),
];
const TTFT_DETAIL_TY: Ty = Ty::Object(TTFT_DETAIL);

const TTFT_FACTS: &[Field] = &[
    req("version", Ty::NumLiteral(1.0)),
    req("observationId", Ty::Uuid),
    req("instanceId", IDENT),
    req("commandId", IDENT),
    opt("sessionId", IDENT),
    opt("turnId", IDENT),
    opt("productTurnId", IDENT),
    opt("queryId", IDENT),
    opt("requestId", IDENT),
    opt("logicalCallId", IDENT),
    opt("cliVersion", IDENT),
    opt("provider", IDENT),
    opt("model", IDENT),
    opt(
        "details",
        Ty::Array {
            item: &TTFT_DETAIL_TY,
            max: 64,
        },
    ),
    opt("truncated", Ty::Bool),
    opt("revision", UINT),
    opt("sendMode", Ty::Enum(&["idle", "queued", "guided"])),
    opt("clockInvalid", Ty::Bool),
    req("receivedAt", UNUM),
    opt("admittedAt", UNUM),
    opt("executionAt", UNUM),
    opt("requestAt", UNUM),
    opt("outputAt", UNUM),
    opt("outputKind", Ty::Enum(&["text", "reasoning", "tool"])),
    opt(
        "terminal",
        Ty::Enum(&[
            "completed",
            "failed",
            "cancelled",
            "rejected",
            "interrupted",
        ]),
    ),
    opt(
        "excluded",
        Ty::Enum(&["busy", "retry", "unsupported", "failed", "capacity"]),
    ),
];

pub fn is_local_ttft_facts(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if !check_object(object, TTFT_FACTS) {
        return false;
    }
    // localTtftDetailSchema.refine：end 缺省或不早于 start。
    object
        .get("details")
        .and_then(Value::as_array)
        .is_none_or(|details| {
            details.iter().all(|detail| match detail["end"].as_f64() {
                Some(end) => detail["start"].as_f64().is_some_and(|start| end >= start),
                None => true,
            })
        })
}

// ── cuaPermissionObservationSchema ──

const CUA_ACCESS_STATUS: &[Field] = &[
    req("schemaVersion", Ty::NumLiteral(1.0)),
    req("platform", Ty::StrLiteral("darwin")),
    req("grantOwner", NONEMPTY),
    req("accessibility", Ty::Enum(&["granted", "stale", "denied"])),
    req(
        "screenRecording",
        Ty::Enum(&["granted", "denied", "unknown"]),
    ),
];

const CUA_OBSERVATION: &[Field] = &[
    req("schemaVersion", Ty::NumLiteral(1.0)),
    req("eventId", NONEMPTY),
    req("eventSeq", UINT),
    req("occurredAt", NUM),
    req("sessionId", NONEMPTY),
    opt("turnId", NONEMPTY),
    req("toolCallId", NONEMPTY),
    req("permissionStatus", Ty::Object(CUA_ACCESS_STATUS)),
];

pub fn is_cua_permission_observation(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| check_object(object, CUA_OBSERVATION))
}

// ── conversationTelemetryFactSchema ──

macro_rules! fact {
    ($kind:literal, $($field:expr),* $(,)?) => {
        &[
            opt("memoryEnabled", Ty::Bool),
            req("version", Ty::NumLiteral(1.0)),
            req("eventId", NONEMPTY),
            req("eventSeq", UINT),
            req("occurredAt", NUM),
            req("sessionId", NONEMPTY),
            opt("sourceCommandId", NONEMPTY),
            opt("turnId", NONEMPTY),
            req("kind", Ty::StrLiteral($kind)),
            $($field),*
        ]
    };
}

const SYNTHETIC_SOURCES: &[&str] = &[
    "background_task",
    "fork",
    "goal_state_change",
    "goal-continuation",
    "plugin_reference",
    "rewind",
    "selection_side_chat",
    "subagent",
    "subagent_message",
    "todo_reminder",
    "workflow_launch",
    "shared_context",
];

const TOOL_PERFORMANCE: &[Field] = &[
    opt("totalMs", UINT),
    opt("permissionWaitMs", UINT),
    opt("commandRunMs", UINT),
    opt("firstOutputMs", UINT),
    opt("noOutputMs", UINT),
    opt("exitCode", INT),
    opt("timedOut", Ty::Bool),
    opt("outputBytes", UINT),
    opt("commandCategory", s(0, Some(64))),
    opt("commandName", s(0, Some(128))),
    opt("commandCount", UINT),
    opt(
        "commandStatus",
        Ty::Enum(&[
            "completed",
            "failed",
            "timed_out",
            "cancelled",
            "spawn_error",
            "backgrounded",
        ]),
    ),
    opt("commandHash", Ty::Hex16),
    opt("fsReadMs", UINT),
    opt("fsWriteMs", UINT),
    opt("patchMatchMs", UINT),
    opt("fileCount", UINT),
    opt("totalBytes", UINT),
    opt("maxFileBytes", UINT),
    opt("hunkCount", UINT),
    opt("matchAttempts", UINT),
    opt("workspaceKind", Ty::Enum(&["local", "remote", "unknown"])),
];

const TURN_STARTED: &[Field] = fact!(
    "turn.started",
    opt("executionKind", Ty::Enum(&["agent", "controlOnly"])),
    opt("inputSource", Ty::Enum(SYNTHETIC_SOURCES)),
    opt(
        "backgroundSource",
        Ty::Enum(&["bash", "subagent", "workflow"])
    ),
    opt("automationId", NONEMPTY),
    opt("offPeakTaskId", NONEMPTY),
    opt("offPeakRunType", Ty::Enum(&["init", "resume"])),
    opt("taskTrigger", Ty::Enum(&["schedule", "manual"])),
    opt("scheduledAt", NUM),
);

const MODEL_REQUEST_STATUS: &[Field] = fact!(
    "model.request.status",
    req("requestId", NONEMPTY),
    req(
        "status",
        Ty::Enum(&[
            "model_request_started",
            "model_request_completed",
            "model_request_failed",
            "model_retry_scheduled",
            "model_stream_stalled",
        ])
    ),
    req("providerId", STR),
    req("modelId", STR),
    opt("providerKind", STR),
    opt("providerHostname", Ty::ProviderHostname),
    req("transport", STR),
    opt("querySource", STR),
    opt("queryId", NONEMPTY),
    req("attempt", UINT),
    req("maxAttempts", UINT),
    opt("durationMs", UNUM),
    opt("reason", STR),
    opt("retryable", Ty::Bool),
    opt("statusCode", INT),
    opt("delayMs", UNUM),
    opt("nextAttempt", UINT),
    opt("idleMs", UNUM),
    opt("timeoutMs", UNUM),
);

const STREAM_CHUNK: &[Field] = fact!(
    "stream.chunk",
    req("channel", Ty::Enum(&["thought", "text"])),
    req("chunkLength", UINT),
    req("firstChunk", Ty::Bool),
    opt("assistantMessageId", NONEMPTY),
    opt("partId", NONEMPTY),
    opt("parentToolCallId", NONEMPTY),
    // Rust runtime 按窗口合并的分片（docs/specs/rust-m9-usage-logs.md §5.7）。
    opt("chunkCount", PINT),
    opt("lastChunkAgeMs", UINT),
);

const TOOL_LIFECYCLE: &[Field] = fact!(
    "tool.lifecycle",
    req(
        "phase",
        Ty::Enum(&["scheduled", "started", "progress", "completed", "failed"])
    ),
    req("toolCallId", NONEMPTY),
    opt("toolName", STR),
    opt("durationMs", UNUM),
    opt("errorCode", STR),
    opt("errorMessage", STR),
    opt("parentToolCallId", NONEMPTY),
    opt("childToolCallId", NONEMPTY),
    opt("agentId", NONEMPTY),
    opt("agentType", STR),
    opt("childSessionId", NONEMPTY),
    opt("background", Ty::Bool),
    opt("skillQualifiedName", NONEMPTY),
    opt("skillPluginId", NONEMPTY),
    opt(
        "skillSource",
        Ty::Enum(&["agents", "zcode", "bundled", "plugin", "remote"])
    ),
    opt("performance", Ty::Object(TOOL_PERFORMANCE)),
    opt("automationId", NONEMPTY),
);

const PERMISSION_LIFECYCLE: &[Field] = fact!(
    "permission.lifecycle",
    req("phase", Ty::Enum(&["requested", "resolved", "denied"])),
    opt("requestId", NONEMPTY),
    req("toolCallId", NONEMPTY),
    opt("toolName", STR),
    opt("childSessionId", NONEMPTY),
    opt("background", Ty::Bool),
    opt(
        "decision",
        Ty::Enum(&["allow", "deny", "escalate", "modify"])
    ),
);

const USAGE_DELTA: &[Field] = fact!(
    "usage.delta",
    opt("requestId", NONEMPTY),
    opt("providerId", STR),
    opt("modelId", STR),
    opt("providerKind", STR),
    opt("providerHostname", Ty::ProviderHostname),
    req("inputTokens", UNUM),
    req("outputTokens", UNUM),
    req("totalTokens", UNUM),
    req("reasoningTokens", UNUM),
    req("cacheReadTokens", UNUM),
    req("cacheWriteTokens", UNUM),
);

const SUBAGENT_LIFECYCLE: &[Field] = fact!(
    "subagent.lifecycle",
    req("phase", Ty::Enum(&["spawned", "stopped"])),
    req("agentId", NONEMPTY),
    opt("agentType", STR),
    req("childSessionId", NONEMPTY),
    opt("parentToolCallId", NONEMPTY),
    req("background", Ty::Bool),
    opt("status", STR),
    opt("errorMessage", STR),
);

const WORKFLOW_LIFECYCLE: &[Field] = fact!(
    "workflow.lifecycle",
    req("phase", Ty::Enum(&["actor-spawned", "run-settled"])),
    req("runId", NONEMPTY),
    opt("toolCallId", NONEMPTY),
    opt("agentId", NONEMPTY),
    opt("childSessionId", NONEMPTY),
    opt("status", Ty::Enum(&["completed", "errored", "stopped"])),
    opt(
        "stopReason",
        Ty::Enum(&["user", "model", "provider", "interrupted", "superseded"])
    ),
    opt("errorMessage", STR),
);

const TURN_TERMINAL: &[Field] = fact!(
    "turn.terminal",
    req("status", Ty::Enum(&["success", "interrupted", "failed"])),
    opt("resultType", STR),
    opt("durationMs", UNUM),
    opt("tokenCount", UNUM),
    opt("toolCallCount", UINT),
    opt("errorCode", STR),
    opt("errorMessage", STR),
    opt("errorRetryable", Ty::Bool),
    opt("turnPhase", STR),
    opt("backgroundSubagentResultConsumed", Ty::Bool),
    opt("workflowResultConsumed", Ty::Bool),
);

const COMPACTION_TERMINAL: &[Field] = fact!(
    "compaction.terminal",
    req("operationId", NONEMPTY),
    opt("messageId", NONEMPTY),
    opt("summaryMessageId", NONEMPTY),
    req("status", Ty::Enum(&["completed", "failed", "interrupted"])),
    req(
        "trigger",
        Ty::Enum(&["manual", "auto", "partial", "reactive", "session_memory"])
    ),
    opt("compactReason", STR),
    opt("reason", STR),
    opt("attempt", PINT),
    opt("maxAttempts", PINT),
    opt("startedAt", NUM),
    opt("endedAt", NUM),
    opt("preCompactTokenCount", UINT),
    opt("postCompactTokenCount", UINT),
    opt("truePostCompactTokenCount", UINT),
    opt("modelName", STR),
    opt("modelProvider", STR),
);

pub fn is_conversation_telemetry_fact(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let fields = match object.get("kind").and_then(Value::as_str) {
        Some("turn.started") => TURN_STARTED,
        Some("model.request.status") => MODEL_REQUEST_STATUS,
        Some("stream.chunk") => STREAM_CHUNK,
        Some("tool.lifecycle") => TOOL_LIFECYCLE,
        Some("permission.lifecycle") => PERMISSION_LIFECYCLE,
        Some("usage.delta") => USAGE_DELTA,
        Some("subagent.lifecycle") => SUBAGENT_LIFECYCLE,
        Some("workflow.lifecycle") => WORKFLOW_LIFECYCLE,
        Some("turn.terminal") => TURN_TERMINAL,
        Some("compaction.terminal") => COMPACTION_TERMINAL,
        _ => return false,
    };
    if !check_object(object, fields) {
        return false;
    }
    let present = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
    };
    // superRefine：只对通过分支校验的事实生效。
    match object["kind"].as_str() {
        Some("turn.started") => {
            let exclusive = !(present("automationId") && present("offPeakTaskId"));
            let run_type_ok = !object.contains_key("offPeakRunType") || present("offPeakTaskId");
            exclusive && run_type_ok
        }
        Some("workflow.lifecycle") => match object["phase"].as_str() {
            Some("actor-spawned") => present("agentId") && present("childSessionId"),
            Some("run-settled") => object.contains_key("status"),
            _ => true,
        },
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TS 生成的样本（scripts/rust-host/agent-schema-fixtures.ts）：每条用真实 zod schema 判定。
    #[test]
    fn matches_typescript_schemas() {
        let cases: Value =
            serde_json::from_str(include_str!("fixtures/schema-cases.json")).expect("fixture json");
        let cases = cases["cases"].as_array().expect("cases");
        assert!(cases.len() > 20);
        for case in cases {
            let input = &case["input"];
            let expected = case["valid"].as_bool().unwrap();
            let actual = match case["schema"].as_str().unwrap() {
                "localTtftFacts" => is_local_ttft_facts(input),
                "cuaPermissionObservation" => is_cua_permission_observation(input),
                "conversationTelemetryFact" => is_conversation_telemetry_fact(input),
                other => panic!("unknown schema {other}"),
            };
            assert_eq!(actual, expected, "{}: {input}", case["name"]);
        }
    }
}
