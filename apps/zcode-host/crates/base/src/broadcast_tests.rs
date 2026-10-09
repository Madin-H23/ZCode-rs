//! broadcast 的扇出与 claim（docs/specs/rust-u4-app-host.md §2.3、§6）。
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use zcode_host_control::{ClientMode, WindowId};
use zcode_host_kit::{CallContext, EventStream, Reply, RpcValue, ServiceChannel};

use super::*;

fn context(connection: u64, window: Option<i64>) -> CallContext {
    CallContext::new(
        connection,
        format!("a{connection}"),
        ClientMode::DesktopContinuous,
    )
    .with_window(window.and_then(WindowId::new))
}

struct Harness {
    channel: Arc<dyn ServiceChannel>,
    broadcaster: Broadcaster,
    now: Arc<AtomicU64>,
}

fn harness() -> Harness {
    let now = Arc::new(AtomicU64::new(1_000));
    let clock_now = now.clone();
    let (builder, broadcaster) =
        service_with_clock(Arc::new(move || clock_now.load(Ordering::SeqCst)));
    Harness {
        channel: builder.build(None),
        broadcaster,
        now,
    }
}

impl Harness {
    async fn call(&self, context: &CallContext, method: &str, arg: Value) -> Value {
        match self
            .channel
            .call(context, method, vec![RpcValue::Json(arg)])
            .await
            .unwrap()
        {
            Reply::Value(value) => value,
            Reply::Undefined => Value::Null,
            other => panic!("unexpected reply {other:?}"),
        }
    }

    fn listen(&self, context: &CallContext) -> EventStream {
        self.channel
            .listen(context, "onMessage", RpcValue::Undefined)
            .unwrap()
    }

    async fn acquire(&self, context: &CallContext, key: &str) -> Value {
        self.call(context, "acquireClaim", json!(key)).await
    }

    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }
}

async fn next(events: &mut EventStream) -> Value {
    match tokio::time::timeout(Duration::from_secs(1), events.next())
        .await
        .expect("broadcast delivered within 1s")
        .expect("event stream open")
    {
        Reply::Value(value) => value,
        other => panic!("unexpected event {other:?}"),
    }
}

fn lease(acquired: &Value) -> Value {
    assert_eq!(acquired["status"], "acquired", "{acquired}");
    acquired["lease"].clone()
}

#[tokio::test]
async fn fan_out_follows_the_listener_window_identity() {
    let harness = harness();
    let (w1, w1_reloaded, w2, phone, other_phone) = (
        context(1, Some(1)),
        context(2, Some(1)),
        context(3, Some(2)),
        context(4, None),
        context(5, None),
    );
    let mut events: Vec<EventStream> = [&w1, &w1_reloaded, &w2, &phone, &other_phone]
        .into_iter()
        .map(|context| harness.listen(context))
        .collect();

    // 窗口 1 发送：窗口 1 的每条连接原样（UI 据此识别自己的消息），其他窗口与无窗口连接带发送方窗口 id。
    let message = json!({"channel": "state:locale", "payload": "en-US"});
    harness.call(&w1, "send", message.clone()).await;
    let received = [
        next(&mut events[0]).await,
        next(&mut events[1]).await,
        next(&mut events[2]).await,
        next(&mut events[3]).await,
        next(&mut events[4]).await,
    ];
    assert_eq!(received[0], message);
    assert_eq!(received[1], message);
    let from_window_1 = json!({"channel": "state:locale", "payload": "en-US", "sourceWindowId": 1});
    assert_eq!(received[2], from_window_1);
    assert_eq!(received[3], from_window_1);
    assert_eq!(received[4], from_window_1);

    // 没有窗口身份的连接发送：自己原样，到达其他连接时 sourceWindowId 为 0。
    let message = json!({"channel": "code-comment:remove", "payload": {"id": "c1"}});
    harness.call(&phone, "send", message.clone()).await;
    let from_phone =
        json!({"channel": "code-comment:remove", "payload": {"id": "c1"}, "sourceWindowId": 0});
    assert_eq!(next(&mut events[0]).await, from_phone);
    assert_eq!(next(&mut events[1]).await, from_phone);
    assert_eq!(next(&mut events[2]).await, from_phone);
    assert_eq!(next(&mut events[3]).await, message);
    assert_eq!(next(&mut events[4]).await, from_phone);

    // Host 内部发送：所有连接都收到原样的消息。
    let message = json!({"channel": "auth:zcode-jwt-invalid", "payload": {}});
    harness.broadcaster.send(message.clone());
    for stream in &mut events {
        assert_eq!(next(stream).await, message);
    }

    // 发送方自带的 sourceWindowId：同窗口原样，其他窗口改写为真实来源（同 BroadcastHub）。
    let message = json!({"channel": "c", "payload": 1, "sourceWindowId": 9});
    harness.call(&w2, "send", message.clone()).await;
    assert_eq!(next(&mut events[2]).await, message);
    assert_eq!(next(&mut events[0]).await["sourceWindowId"], 2);
}

#[tokio::test]
async fn send_rejects_invalid_messages() {
    let harness = harness();
    for invalid in [
        json!({"payload": 1}),
        json!({"channel": " ", "payload": 1}),
        json!({"channel": "c", "sourceWindowId": "1"}),
        json!("c"),
    ] {
        let error = harness
            .channel
            .call(&context(1, Some(1)), "send", vec![RpcValue::Json(invalid)])
            .await
            .unwrap_err();
        assert_eq!(error.name, "ZodError");
    }
}

#[tokio::test]
async fn claims_are_first_wins_across_windows_and_expire() {
    let harness = harness();
    let (w1, w2) = (context(1, Some(1)), context(3, Some(2)));
    let first = harness.acquire(&w1, " reset:week ").await;
    assert_eq!(first["lease"]["key"], "reset:week", "key is trimmed");
    lease(&first);
    // 其他窗口在 reservation 期间拿到 busy，重试提示不超过 250 ms，也不超过剩余有效期。
    assert_eq!(
        harness.acquire(&w2, "reset:week").await,
        json!({"status": "busy", "retryAfterMs": 250})
    );
    harness.advance(4_900);
    assert_eq!(
        harness.acquire(&w2, "reset:week").await,
        json!({"status": "busy", "retryAfterMs": 100})
    );
    // 5 s 到期：reservation 被回收，下一个申请方拿到。
    harness.advance(100);
    lease(&harness.acquire(&w2, "reset:week").await);
    // 空 key 不可用。
    assert_eq!(
        harness.acquire(&w1, "  ").await,
        json!({"status": "unavailable"})
    );
}

#[tokio::test]
async fn only_the_holder_commits_or_releases_and_commits_are_kept() {
    let harness = harness();
    let (holder, same_window, other) = (
        context(1, Some(1)),
        context(2, Some(1)),
        context(3, Some(2)),
    );
    let held = lease(&harness.acquire(&holder, "k").await);

    // 别的连接（即使同一窗口）拿着 token 也不能 commit / release；持有方用错 token 也不行。
    harness
        .call(&same_window, "commitClaim", held.clone())
        .await;
    harness.call(&other, "releaseClaim", held.clone()).await;
    harness
        .call(
            &holder,
            "commitClaim",
            json!({"key": "k", "token": "forged"}),
        )
        .await;
    assert_eq!(harness.acquire(&other, "k").await["status"], "busy");

    // 持有方 commit：之后任何人都得到 committed，过期与 release 都不影响。
    harness.call(&holder, "commitClaim", held.clone()).await;
    assert_eq!(
        harness.acquire(&other, "k").await,
        json!({"status": "committed"})
    );
    harness.call(&holder, "releaseClaim", held).await;
    harness.advance(60_000);
    assert_eq!(
        harness.acquire(&holder, "k").await,
        json!({"status": "committed"})
    );

    // 持有方 release 未 commit 的 reservation：其他申请方立即可以拿到。
    let held = lease(&harness.acquire(&holder, "k2").await);
    harness.call(&other, "releaseClaim", held.clone()).await;
    assert_eq!(harness.acquire(&other, "k2").await["status"], "busy");
    harness.call(&holder, "releaseClaim", held).await;
    lease(&harness.acquire(&other, "k2").await);

    // tryClaim：拿到即 commit；之后同 key 都是 false。
    assert_eq!(harness.call(&other, "tryClaim", json!("once")).await, true);
    assert_eq!(
        harness.call(&holder, "tryClaim", json!("once")).await,
        false
    );
    assert_eq!(harness.call(&other, "tryClaim", json!("once")).await, false);
}

#[tokio::test]
async fn closing_a_connection_releases_only_its_uncommitted_reservations() {
    let harness = harness();
    let (closing, staying, other) = (
        context(1, Some(1)),
        context(2, Some(1)),
        context(3, Some(2)),
    );
    lease(&harness.acquire(&closing, "reserved").await);
    let committed = lease(&harness.acquire(&closing, "committed").await);
    harness.call(&closing, "commitClaim", committed).await;
    lease(&harness.acquire(&staying, "other-connection").await);

    harness.channel.connection_closed(&closing);
    lease(&harness.acquire(&other, "reserved").await);
    assert_eq!(
        harness.acquire(&other, "committed").await,
        json!({"status": "committed"})
    );
    // 同一窗口的其他连接的 reservation 不受影响（申请方是连接，不是窗口）。
    assert_eq!(
        harness.acquire(&other, "other-connection").await["status"],
        "busy"
    );
}

#[test]
fn the_claim_table_evicts_the_oldest_entry_over_its_cap() {
    let claims = Claims::new(Arc::new(|| 0));
    for index in 0..MAX_CLAIMS {
        let acquired = claims.acquire(1, &format!("k{index}"));
        let lease = Lease {
            key: acquired["lease"]["key"].as_str().unwrap().to_owned(),
            token: acquired["lease"]["token"].as_str().unwrap().to_owned(),
        };
        claims.commit(1, &lease);
    }
    assert_eq!(claims.len(), MAX_CLAIMS);
    // 第 1025 条把最早的 k0 挤掉（committed 也会被淘汰，同 BroadcastHub）。
    assert_eq!(claims.acquire(1, "new")["status"], "acquired");
    assert_eq!(claims.len(), MAX_CLAIMS);
    assert_eq!(claims.acquire(2, "k1")["status"], "committed");
    assert_eq!(claims.acquire(2, "k0")["status"], "acquired");
}
