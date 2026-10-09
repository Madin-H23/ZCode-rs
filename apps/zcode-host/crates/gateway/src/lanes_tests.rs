use std::sync::Arc;

use super::lanes::Lanes;

#[tokio::test]
async fn the_lane_passes_to_waiters_in_entry_order() {
    let lanes = Arc::new(Lanes::default());
    let mut first = lanes.enter("k".into(), 1);
    let mut second = lanes.enter("k".into(), 2);
    let mut third = lanes.enter("k".into(), 1);
    assert!(first.turn().await);
    let second_turn = tokio::spawn(async move { (second.turn().await, second) });
    tokio::task::yield_now().await;
    assert!(!second_turn.is_finished(), "the second waits for the first");
    drop(first);
    let (admitted, second) = second_turn.await.unwrap();
    assert!(admitted);
    drop(second);
    assert!(third.turn().await);
    drop(third);
    assert!(lanes.is_empty(), "an idle lane is removed");
}

#[tokio::test]
async fn different_keys_do_not_wait_for_each_other() {
    let lanes = Arc::new(Lanes::default());
    let mut a = lanes.enter("a".into(), 1);
    let mut b = lanes.enter("b".into(), 1);
    assert!(a.turn().await);
    assert!(b.turn().await);
}

#[tokio::test]
async fn withdrawn_waiters_never_get_the_lane() {
    let lanes = Arc::new(Lanes::default());
    let holder = lanes.enter("k".into(), 1);
    let mut cancelled = lanes.enter("k".into(), 1);
    let mut closed = lanes.enter("k".into(), 2);
    let mut next = lanes.enter("k".into(), 3);
    lanes.withdraw("k", cancelled.id());
    lanes.withdraw_connection(2);
    assert!(!cancelled.turn().await);
    assert!(!closed.turn().await);
    // 撤出的 Ticket 释放时不影响通道。
    drop(cancelled);
    drop(closed);
    drop(holder);
    assert!(next.turn().await);
    drop(next);
    assert!(lanes.is_empty());
}

#[tokio::test]
async fn a_waiter_dropped_before_its_turn_leaves_the_queue() {
    let lanes = Arc::new(Lanes::default());
    let holder = lanes.enter("k".into(), 1);
    let dropped = lanes.enter("k".into(), 1);
    let mut next = lanes.enter("k".into(), 1);
    drop(dropped);
    drop(holder);
    assert!(next.turn().await);
}

#[tokio::test]
async fn an_admitted_waiter_that_never_ran_passes_the_lane_on() {
    let lanes = Arc::new(Lanes::default());
    let holder = lanes.enter("k".into(), 1);
    let admitted = lanes.enter("k".into(), 1);
    let mut next = lanes.enter("k".into(), 1);
    // 通道交给 `admitted` 时它的任务已被中止、还没来得及观察：它释放时通道继续往下交。
    drop(holder);
    drop(admitted);
    assert!(next.turn().await);
}
