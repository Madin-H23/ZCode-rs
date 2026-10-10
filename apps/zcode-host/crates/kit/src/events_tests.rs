use futures_util::StreamExt;

use super::*;

const SOON: std::time::Duration = std::time::Duration::from_secs(5);

#[tokio::test]
async fn delivers_to_matching_subscribers_and_drops_closed_ones() {
    let hub = EventHub::<u32>::new("test", 16, Overflow::CloseSubscriber);
    let mut all = Box::pin(hub.subscribe());
    let mut even = Box::pin(hub.subscribe_filtered(|value| value % 2 == 0));
    let dropped = hub.subscribe();
    drop(dropped);
    hub.emit(1);
    hub.emit(2);
    assert_eq!(all.next().await, Some(1));
    assert_eq!(all.next().await, Some(2));
    assert_eq!(even.next().await, Some(2));
    assert_eq!(hub.subscriber_count(), 2);
}

#[tokio::test]
async fn close_ends_existing_streams() {
    let hub = EventHub::<u32>::new("test", 16, Overflow::CloseSubscriber);
    let mut events = Box::pin(hub.subscribe());
    hub.emit(1);
    hub.close();
    hub.emit(2);
    assert_eq!(events.next().await, Some(1));
    assert_eq!(events.next().await, None);
}

/// 落后 `capacity` 条的订阅者：已入队的照常读完，然后流结束；跟得上的订阅者不受影响。
#[tokio::test]
async fn a_subscriber_that_falls_behind_is_closed_without_affecting_the_others() {
    let hub = EventHub::<u32>::new("test", 2, Overflow::CloseSubscriber);
    let mut slow = Box::pin(hub.subscribe());
    let mut fast = Box::pin(hub.subscribe());
    for value in 0..3 {
        hub.emit(value);
        assert_eq!(fast.next().await, Some(value));
    }
    assert_eq!(slow.next().await, Some(0));
    assert_eq!(slow.next().await, Some(1));
    assert_eq!(slow.next().await, None);
    hub.emit(3);
    assert_eq!(fast.next().await, Some(3));
    assert_eq!(hub.subscriber_count(), 1);
}

/// 只表示“有变化”的信号：队列里已有一条时新的被丢弃，订阅者保留。
#[tokio::test]
async fn coalescing_hubs_keep_one_pending_signal() {
    let hub = EventHub::<()>::new("test", 1, Overflow::Coalesce);
    let mut changes = Box::pin(hub.subscribe());
    for _ in 0..5 {
        hub.emit(());
    }
    assert_eq!(changes.next().await, Some(()));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), changes.next())
            .await
            .is_err(),
        "the extra signals were merged"
    );
    hub.emit(());
    assert_eq!(changes.next().await, Some(()));
    assert_eq!(hub.subscriber_count(), 1);
}

/// 能等待的发送方：订阅者的队列满时 `emit_waiting` 等它读走一条，事件一条不丢、顺序不变。
#[tokio::test]
async fn emit_waiting_applies_backpressure() {
    let hub = EventHub::<u32>::new("test", 1, Overflow::CloseSubscriber);
    let mut events = Box::pin(hub.subscribe());
    hub.emit_waiting(0).await;
    let producer = {
        let hub = hub.clone();
        zcode_async::owned(async move {
            for value in 1..4 {
                hub.emit_waiting(value).await;
            }
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!producer.is_finished(), "the producer waits for room");
    for value in 0..4 {
        assert_eq!(
            tokio::time::timeout(SOON, events.next()).await.unwrap(),
            Some(value)
        );
    }
    tokio::time::timeout(SOON, producer).await.unwrap().unwrap();
}
