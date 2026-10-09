//! The input queue as seen from outside the runtime (spec rust-u3-stage-b
//! §3.4.5): `v4/conversation/inputQueue`, the input a run is serving and the
//! sessions-index summary that carries it. Host task commands (enqueue /
//! promote / cancel) read the `CommandInbox` through these instead of keeping
//! a queue of their own.
use super::Engine;
use crate::domain::legacy_params::ParamsError;
use crate::domain::session::Session;
use crate::domain::zod::{Schema, non_empty, protocol_error};
use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The header of the running turn. A run reserves `run_id` and appends its
/// header in the same step, so while running the last header is the running one.
fn running_header(s: &Session) -> Option<(usize, &Value)> {
    if !s.running() {
        return None;
    }
    let at = s.rows.iter().rposition(|r| r["kind"] == "turnHeader")?;
    Some((at, &s.rows[at]))
}

/// The command that submitted the running turn (legacy `TurnStarted.inputId`);
/// `None` when idle or the turn has no source command (goal loop, resumes).
fn running_input(s: &Session) -> Option<&str> {
    running_header(s)?.1["sourceCommandId"]
        .as_str()
        .filter(|id| !id.is_empty())
}

/// `activeInput`: the running turn's source command, the submitter's
/// `externalId`, its start time and the text of the turn's first input row.
fn active_input(s: &Session) -> Option<Value> {
    let (at, header) = running_header(s)?;
    let mut active = json!({"inputId": running_input(s)?});
    for key in ["externalId", "startedAt"] {
        if let Some(value) = header.get(key).filter(|v| v.is_string() || v.is_number()) {
            active[key] = value.clone();
        }
    }
    let text = s.rows[at..]
        .iter()
        .find(|r| r["kind"] == "userInput" && r["turnId"] == header["turnId"])
        .and_then(|r| r.get("text"))
        .filter(|text| text.is_string());
    if let Some(text) = text {
        active["text"] = text.clone();
    }
    Some(active)
}

/// The sessions-index summary of `s`: the domain summary plus `activeInputId`,
/// which changes together with the phase (no extra frames).
pub(super) fn index_summary(s: &Session) -> Value {
    let mut summary = s.summary();
    if let Some(input) = running_input(s) {
        summary["activeInputId"] = input.into();
    }
    summary
}

impl Engine {
    /// `v4/conversation/inputQueue`: read-only; `queue.items` is the whole
    /// `CommandInbox` queue, including startNow reservations that snapshots hide.
    /// `running` is true for any run, also one without `activeInput` (goal loop).
    pub(super) async fn input_queue(&mut self, p: &Value) -> Result<Value> {
        let id = p["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("Session id required")?;
        self.ensure_session(id).await?;
        let s = self.sessions.get(id).context("Session unavailable")?;
        let mut result = json!({"revision": s.revision, "running": s.running(),
            "queue": {"items": s.queue, "autoDrain": s.auto_drain}});
        if let Some(active) = active_input(s) {
            result["activeInput"] = active;
        }
        Ok(result)
    }
}

/// The submitter's name for an input (`sendText.externalId`), copied onto its
/// queue item or turn header; it never enters the persisted intent.
pub(super) fn copy_external_id(from: &Value, to: &mut Value) {
    if let Some(external) = from.get("externalId").filter(|v| v.is_string()) {
        to["externalId"] = external.clone();
    }
}

/// The queue options of legacy `session/send` (`requestedDelivery: "queue"`,
/// `externalId`), validated here and removed before the zod-parity parser of
/// the remaining params.
#[derive(Default)]
pub(super) struct LegacyQueueOptions {
    /// Queue while running, even under the legacy lock.
    pub requested: bool,
    pub external_id: Option<String>,
}

pub(super) fn legacy_queue_options(
    raw: &Value,
) -> std::result::Result<(Value, LegacyQueueOptions), ParamsError> {
    let mut rest = raw.clone();
    let Some(params) = rest.as_object_mut() else {
        return Ok((rest, LegacyQueueOptions::default()));
    };
    let mut options = serde_json::Map::new();
    for key in ["requestedDelivery", "externalId"] {
        if let Some(value) = params.remove(key) {
            options.insert(key.into(), value);
        }
    }
    let schema = Schema::Object(vec![
        ("requestedDelivery", Schema::Literal(json!("queue")), true),
        ("externalId", non_empty(), true),
    ]);
    let (parsed, issues) = schema.run(Some(&Value::Object(options)));
    if !issues.is_empty() {
        let (message, data) = protocol_error(&issues);
        return Err(ParamsError { message, data });
    }
    let options = LegacyQueueOptions {
        requested: parsed.get("requestedDelivery").is_some(),
        external_id: parsed["externalId"].as_str().map(str::to_owned),
    };
    Ok((rest, options))
}
