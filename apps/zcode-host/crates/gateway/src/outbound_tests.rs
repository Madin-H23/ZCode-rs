use std::sync::Arc;

use tokio::sync::watch;
use zcode_host_kit::FlowState;

use super::outbound::{CloseReason, CloseSwitch, InUse, Outbound, OutboundLimits, Queue};

fn queue(
    high: usize,
    low: usize,
    hard: usize,
) -> (Outbound, watch::Receiver<FlowState>, CloseSwitch) {
    let close = CloseSwitch::new();
    let limits = OutboundLimits {
        high_water: high,
        low_water: low,
        hard_limit: hard,
    };
    let (queue, edges) = Queue::new(1, limits, close.clone());
    (
        Outbound::new(queue, Arc::<InUse>::default().hold()),
        edges,
        close,
    )
}

#[test]
fn edges_fire_once_per_crossing() {
    let (outbound, mut edges, _) = queue(100, 25, usize::MAX);
    let queue = outbound.queue.clone();
    queue.push(vec![0; 60]);
    queue.push(vec![0; 40]);
    assert!(
        !edges.has_changed().unwrap(),
        "100 bytes is not above the mark"
    );
    queue.push(vec![0; 1]);
    assert!(edges.has_changed().unwrap());
    assert_eq!(*edges.borrow_and_update(), FlowState::Saturated);
    queue.push(vec![0; 500]);
    assert!(!edges.has_changed().unwrap(), "no repeated saturated");
    outbound.written(501);
    assert!(
        !edges.has_changed().unwrap(),
        "100 bytes left, still above low"
    );
    outbound.written(75);
    assert_eq!(*edges.borrow_and_update(), FlowState::Drained);
    assert_eq!(queue.backlog(), 25);
    outbound.written(25);
    assert!(!edges.has_changed().unwrap(), "no repeated drained");
    outbound.written(10);
    assert_eq!(queue.backlog(), 0);
}

#[tokio::test]
async fn messages_leave_in_queue_order_and_close_ends_the_stream() {
    let (outbound, _, _) = queue(100, 25, usize::MAX);
    let queue = outbound.queue.clone();
    assert!(queue.push(vec![1]));
    assert!(queue.push(vec![2]));
    assert_eq!(outbound.next().await, Some(vec![1]));
    queue.close();
    assert!(!queue.push(vec![3]), "a closed queue drops messages");
    // 关闭时还没取出的消息随连接一起丢弃。
    assert_eq!(outbound.next().await, None);
}

#[tokio::test]
async fn a_waiting_consumer_wakes_for_new_messages_and_for_close() {
    let (outbound, _, _) = queue(100, 25, usize::MAX);
    let queue = outbound.queue.clone();
    let waiting = tokio::spawn(async move {
        let first = outbound.next().await;
        let second = outbound.next().await;
        (first, second)
    });
    tokio::task::yield_now().await;
    assert!(queue.push(vec![1]));
    tokio::task::yield_now().await;
    queue.close();
    assert_eq!(waiting.await.unwrap(), (Some(vec![1]), None));
}

#[tokio::test]
async fn exceeding_the_hard_limit_closes_the_connection() {
    let (outbound, _, close) = queue(100, 25, 1000);
    let mut closed = close.subscribe();
    let queue = outbound.queue.clone();
    assert!(queue.push(vec![0; 600]));
    assert_eq!(outbound.close_reason(), None);
    assert!(!queue.push(vec![0; 600]), "1200 bytes is above the limit");
    assert_eq!(
        *closed.borrow_and_update(),
        Some(CloseReason::OutboundBacklog)
    );
    assert_eq!(outbound.next().await, None);
    assert_eq!(outbound.close_reason(), Some(CloseReason::OutboundBacklog));
    assert!(!queue.push(vec![0; 1]));
}

#[tokio::test]
async fn a_failed_transport_write_closes_the_connection() {
    let (outbound, _, close) = queue(100, 25, 1000);
    let closed = close.subscribe();
    outbound.queue.push(vec![1]);
    outbound.close();
    assert_eq!(*closed.borrow(), Some(CloseReason::TransportFailed));
    assert_eq!(outbound.next().await, None);
    assert_eq!(outbound.close_reason(), Some(CloseReason::TransportFailed));
}

/// 多个关闭触发时第一个决定原因（docs/specs/rust-u2-gateway.md §2.6）：之后越过上限、写失败都不改写它。
#[tokio::test]
async fn the_first_trigger_decides_the_reason() {
    let (outbound, _, close) = queue(100, 25, 1000);
    close.close(CloseReason::Detached);
    assert!(!outbound.queue.push(vec![0; 2000]));
    outbound.close();
    assert_eq!(outbound.next().await, None);
    assert_eq!(outbound.close_reason(), Some(CloseReason::Detached));
    assert_eq!(close.reason(), Some(CloseReason::Detached));

    let (outbound, _, close) = queue(100, 25, 1000);
    assert!(!outbound.queue.push(vec![0; 2000]));
    close.close(CloseReason::Detached);
    outbound.close();
    assert_eq!(outbound.close_reason(), Some(CloseReason::OutboundBacklog));
}

#[test]
fn reasons_are_short_ascii_tokens() {
    for (reason, text) in [
        (CloseReason::OutboundBacklog, "outbound-backlog"),
        (CloseReason::Detached, "detached"),
        (CloseReason::ScopeUnavailable, "scope-unavailable"),
        (CloseReason::ScopeClosed, "scope-closed"),
        (CloseReason::HostShutdown, "host-shutdown"),
        (CloseReason::PeerClosed, "peer-closed"),
        (CloseReason::TransportFailed, "transport-failed"),
        (CloseReason::InternalError, "internal-error"),
    ] {
        assert_eq!(reason.as_str(), text);
    }
}
