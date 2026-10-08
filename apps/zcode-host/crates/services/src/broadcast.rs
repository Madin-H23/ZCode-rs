//! `broadcast` channel（packages/services/src/broadcast/broadcastService.ts）。
//!
//! send：校验后本地触发 onMessage，再交给 Main 的 BroadcastHub 转发到其他窗口。
//! claim：向 Main 请求跨窗口原子 claim，2 s 内无结果视为 unavailable（不能乐观放行，
//! Main 可能已把同一 key 授予别的窗口）；迟到的 acquired 结果立即 release。
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use zcode_host_control::ControlRouter;
use zcode_host_kit::{EventHub, Reply, RpcError, ServiceBuilder, replies};

use crate::main_bridge::{MainBridge, random_id};

const CLAIM_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lease {
    key: String,
    token: String,
}

fn validate_message(message: Value) -> Result<Value, RpcError> {
    let channel_ok = message
        .get("channel")
        .and_then(Value::as_str)
        .is_some_and(|channel| !channel.trim().is_empty());
    let source_ok = match message.get("sourceWindowId") {
        None | Some(Value::Null) => true,
        Some(value) => value.as_i64().is_some(),
    };
    if !message.is_object() || !channel_ok || !source_ok {
        return Err(RpcError::new("ZodError", "invalid broadcast message"));
    }
    Ok(message)
}

/// Host 内部发广播（例如账号的 `auth:zcode-jwt-invalid`）：与 `send` 相同，先本地触发
/// onMessage，再交给 Main 转发到其他窗口。
#[derive(Clone)]
pub struct Broadcaster {
    messages: EventHub<Value>,
    bridge: MainBridge,
}

impl Broadcaster {
    pub fn send(&self, message: Value) {
        self.messages.emit(message.clone());
        self.bridge
            .post(json!({"type": "broadcast", "message": message}));
    }
}

pub fn service(bridge: MainBridge, router: &mut ControlRouter) -> ServiceBuilder {
    service_with_sender(bridge, router).0
}

pub fn service_with_sender(
    bridge: MainBridge,
    router: &mut ControlRouter,
) -> (ServiceBuilder, Broadcaster) {
    let messages = EventHub::<Value>::new();
    let broadcaster = Broadcaster {
        messages: messages.clone(),
        bridge: bridge.clone(),
    };
    {
        let messages = messages.clone();
        router.on("broadcast", move |envelope| {
            if let Some(message) = envelope.get("message").cloned()
                && let Ok(message) = validate_message(message)
            {
                messages.emit(message);
            }
        });
    }
    {
        let bridge = bridge.clone();
        router.on("broadcast-claim-result", move |result| {
            if let Some(request_id) = result.get("requestId").and_then(Value::as_str) {
                bridge.resolve(request_id, result.clone());
            }
        });
    }

    let acquire = {
        let bridge = bridge.clone();
        move |key: String| {
            let bridge = bridge.clone();
            async move { acquire_claim(&bridge, key).await }
        }
    };
    let try_acquire = acquire.clone();
    let commit_bridge = bridge.clone();
    let release_bridge = bridge.clone();
    let try_bridge = bridge.clone();
    let send_messages = messages.clone();

    let builder = ServiceBuilder::new("broadcast")
        .method("send", move |_, args| {
            let result = args
                .get::<Value>(0, "message")
                .and_then(validate_message)
                .map(|message| {
                    send_messages.emit(message.clone());
                    bridge.post(json!({"type": "broadcast", "message": message}));
                });
            async move { result.map(|()| Reply::Undefined) }
        })
        .method("acquireClaim", move |_, args| {
            let key = args.get::<String>(0, "key");
            let acquire = acquire.clone();
            async move { Ok(Reply::from(acquire(key?).await)) }
        })
        .method("commitClaim", move |_, args| {
            let result = args
                .get::<Lease>(0, "lease")
                .map(|lease| post_claim(&commit_bridge, "broadcast-claim-commit", &lease));
            async move { result.map(|()| Reply::Undefined) }
        })
        .method("releaseClaim", move |_, args| {
            let result = args
                .get::<Lease>(0, "lease")
                .map(|lease| post_claim(&release_bridge, "broadcast-claim-release", &lease));
            async move { result.map(|()| Reply::Undefined) }
        })
        .method("tryClaim", move |_, args| {
            let key = args.get::<String>(0, "key");
            let (acquire, bridge) = (try_acquire.clone(), try_bridge.clone());
            async move {
                let result = acquire(key?).await;
                if result["status"] != "acquired" {
                    return Ok(Reply::from(Value::Bool(false)));
                }
                let lease = Lease {
                    key: result["lease"]["key"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    token: result["lease"]["token"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                };
                post_claim(&bridge, "broadcast-claim-commit", &lease);
                Ok(Reply::from(Value::Bool(true)))
            }
        })
        .event("onMessage", move |_, _| Ok(replies(messages.subscribe())));
    (builder, broadcaster)
}

fn post_claim(bridge: &MainBridge, kind: &str, lease: &Lease) {
    bridge.post(json!({"type": kind, "key": lease.key, "claimToken": lease.token}));
}

async fn acquire_claim(bridge: &MainBridge, key: String) -> Value {
    let key = key.trim().to_owned();
    if key.is_empty() {
        return json!({"status": "unavailable"});
    }
    let request_id = random_id();
    let release_bridge = bridge.clone();
    let release_key = key.clone();
    let result = bridge
        .request(
            request_id.clone(),
            json!({"type": "broadcast-claim-request", "requestId": request_id, "key": key}),
            CLAIM_TIMEOUT,
            move |late| {
                if late["status"] == "acquired"
                    && let Some(token) = late["claimToken"].as_str()
                {
                    post_claim(
                        &release_bridge,
                        "broadcast-claim-release",
                        &Lease {
                            key: release_key,
                            token: token.to_owned(),
                        },
                    );
                }
            },
        )
        .await;
    match result {
        Some(result) if result["status"] == "acquired" => json!({
            "status": "acquired",
            "lease": {"key": key, "token": result["claimToken"]},
        }),
        Some(result) if result["status"] == "busy" => json!({
            "status": "busy",
            "retryAfterMs": result["retryAfterMs"],
        }),
        Some(result) if result["status"] == "committed" => json!({"status": "committed"}),
        _ => json!({"status": "unavailable"}),
    }
}
