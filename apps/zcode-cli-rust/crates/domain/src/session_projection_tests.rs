use crate::execution::Phase;
use crate::session::{ForegroundRun, Session};
use serde_json::json;

/// Node `onTurnStarted`：`activeWorks[].startedAt` 是轮次开始的时间；之后的事件（流式分片等）
/// 只推进会话的 `updated_at`，control 不变。
#[test]
fn control_keeps_the_turn_start_while_events_advance_the_session() {
    let mut s = Session::new(
        "s".into(),
        "w".into(),
        "p".into(),
        "m".into(),
        "none".into(),
        "e".into(),
        1,
    );
    s.phase = Phase::Running;
    s.run = Some(ForegroundRun {
        id: "run-1".into(),
        started_at: 5,
    });
    s.updated_at = 5;
    let control = s.patch()["control"].clone();
    assert_eq!(
        control["activeWorks"],
        json!([{"kind": "primaryTurn", "foregroundExecutionId": "run-1", "startedAt": 5}])
    );
    s.updated_at = 9;
    assert_eq!(s.patch()["control"], control);
}
