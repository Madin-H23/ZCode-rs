//! RPC 网络遥测（`@zcode/rpc` 的 `NetworkTelemetryChannelServer` 与 desktop
//! `hostNetworkTelemetry.ts`；docs/specs/rust-host-integration.md §2.3）。
//!
//! 分发层在每次 `call` 完成时记一条 `channel.command` 级观测；[`RpcTelemetry`] 是观测缓冲的唯一
//! 所有者：每 30 s 或累计 200 条发一批 `network-telemetry-batch` 给 Main，Host 退出前冲刷一次。
//! 上报端点由 Main 决定，Host 不出网。
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Map, Value, json};
use zcode_host_control::{ControlSink, HostToMain};
use zcode_host_kit::NOT_IMPLEMENTED_CODE;
use zcode_host_rpc::RpcError;

pub const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
pub const FLUSH_MAX_BATCH: usize = 200;

/// `classifyErrorKind`：按错误消息（小写）归类。
pub fn classify_error_kind(message: &str) -> &'static str {
    let message = message.to_lowercase();
    if message.contains("timeout") || message.contains("timed out") {
        return "timeout";
    }
    if message.contains("dns") || message.contains("getaddrinfo") || message.contains("enotfound") {
        return "dns_failure";
    }
    if message.contains("econnreset")
        || message.contains("connection reset")
        || message.contains("econnrefused")
    {
        return "connection_reset";
    }
    "other"
}

/// `emitRpcObservation`：键序同 TS 对象字面量，成功时省略 `errorKind`。
pub fn observation(
    channel: &str,
    command: &str,
    duration: Duration,
    error: Option<&RpcError>,
) -> Value {
    let mut out = Map::new();
    out.insert("transport".into(), json!("rpc"));
    out.insert("interface".into(), json!(format!("{channel}.{command}")));
    out.insert(
        "durationMs".into(),
        json!((duration.as_secs_f64() * 1000.0).round().max(0.0) as u64),
    );
    out.insert("ok".into(), json!(error.is_none()));
    if let Some(error) = error {
        out.insert(
            "errorKind".into(),
            json!(classify_error_kind(&error.message)),
        );
    }
    out.insert("attempt".into(), json!(1));
    Value::Object(out)
}

/// 契约桩的应答：成员尚未实现，不是业务调用失败。
fn is_stub_reply(error: &RpcError) -> bool {
    error.code.as_deref() == Some(&Value::String(NOT_IMPLEMENTED_CODE.to_owned()))
}

#[derive(Default)]
struct State {
    pending: Vec<Value>,
    timer_started: bool,
    stopped: bool,
}

pub struct RpcTelemetry {
    control: Arc<dyn ControlSink>,
    state: Mutex<State>,
}

impl RpcTelemetry {
    pub fn new(control: Arc<dyn ControlSink>) -> Arc<Self> {
        Arc::new(Self {
            control,
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 记录一次已完成的调用；累计满一批时立即发出。
    pub fn record(
        self: &Arc<Self>,
        channel: &str,
        command: &str,
        duration: Duration,
        error: Option<&RpcError>,
    ) {
        if error.is_some_and(is_stub_reply) {
            return;
        }
        let full = {
            let mut state = self.lock();
            if state.stopped {
                return;
            }
            state
                .pending
                .push(observation(channel, command, duration, error));
            if !state.timer_started {
                state.timer_started = true;
                self.start_timer();
            }
            state.pending.len() >= FLUSH_MAX_BATCH
        };
        if full {
            self.flush();
        }
    }

    fn start_timer(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak: Weak<Self> = Arc::downgrade(self);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(FLUSH_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(telemetry) = weak.upgrade() else {
                    return;
                };
                if telemetry.lock().stopped {
                    return;
                }
                telemetry.flush();
            }
        });
    }

    /// `flushHostNetworkTelemetryBatch`：一次最多发 200 条；发送失败不影响 Host 主流程。
    pub fn flush(&self) {
        let observations: Vec<Value> = {
            let mut state = self.lock();
            if state.pending.is_empty() {
                return;
            }
            let take = state.pending.len().min(FLUSH_MAX_BATCH);
            state.pending.drain(..take).collect()
        };
        self.control.send(HostToMain::Raw(json!({
            "type": "network-telemetry-batch",
            "observations": observations,
        })));
    }

    /// `stopHostNetworkTelemetry`：停止计时、冲刷剩余观测，之后的调用不再记录。
    pub fn shutdown(&self) {
        self.flush();
        self.lock().stopped = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Value>>);

    impl ControlSink for Recorder {
        fn send(&self, message: HostToMain) {
            if let HostToMain::Raw(value) = message {
                self.0.lock().unwrap().push(value);
            }
        }
    }

    #[test]
    fn observations_match_the_typescript_shape() {
        assert_eq!(
            observation("setting", "get", Duration::from_micros(1_600), None),
            json!({"transport": "rpc", "interface": "setting.get", "durationMs": 2, "ok": true, "attempt": 1})
        );
        let error = RpcError::new("Error", "Request timed out after 5ms");
        assert_eq!(
            observation("git", "status", Duration::ZERO, Some(&error)).to_string(),
            r#"{"transport":"rpc","interface":"git.status","durationMs":0,"ok":false,"errorKind":"timeout","attempt":1}"#
        );
        assert_eq!(
            classify_error_kind("getaddrinfo ENOTFOUND x"),
            "dns_failure"
        );
        assert_eq!(classify_error_kind("read ECONNRESET"), "connection_reset");
        assert_eq!(
            classify_error_kind("connect ECONNREFUSED"),
            "connection_reset"
        );
        assert_eq!(classify_error_kind("boom"), "other");
    }

    #[tokio::test]
    async fn batches_flush_at_200_and_on_shutdown_and_skip_stub_replies() {
        let recorder = Arc::new(Recorder::default());
        let telemetry = RpcTelemetry::new(recorder.clone());
        let stub = RpcError::new("NotImplemented", "x").with_code(NOT_IMPLEMENTED_CODE);
        telemetry.record("a", "stub", Duration::ZERO, Some(&stub));
        for index in 0..201 {
            telemetry.record("a", &format!("m{index}"), Duration::ZERO, None);
        }
        {
            let sent = recorder.0.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0]["type"], "network-telemetry-batch");
            let observations = sent[0]["observations"].as_array().unwrap();
            assert_eq!(observations.len(), 200);
            assert_eq!(observations[0]["interface"], "a.m0");
        }
        telemetry.shutdown();
        telemetry.record("a", "late", Duration::ZERO, None);
        telemetry.flush();
        let sent = recorder.0.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[1]["observations"],
            json!([observation("a", "m200", Duration::ZERO, None)])
        );
    }
}
