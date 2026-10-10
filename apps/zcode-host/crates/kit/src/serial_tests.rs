use std::sync::{Arc, Mutex};
use std::time::Duration;

use zcode_async::{TaskError, TaskScope, owned};

use super::*;

fn scope() -> TaskScope {
    TaskScope::root("test", |_| {})
}

/// 提交顺序即执行顺序：后提交的工作在前一个结束之后才开始，即使前一个在等待。
#[tokio::test]
async fn jobs_run_one_at_a_time_in_submission_order() {
    let scope = scope();
    let serial = Serial::start(&scope.spawner(), "writes", 8);
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut submitted = Vec::new();
    for index in 0..5u64 {
        let (serial, log) = (serial.clone(), log.clone());
        submitted.push(async move {
            serial
                .run(async move {
                    log.lock().unwrap().push(format!("start {index}"));
                    tokio::time::sleep(Duration::from_millis(5 * (5 - index))).await;
                    log.lock().unwrap().push(format!("end {index}"));
                    index
                })
                .await
        });
    }
    let results = futures_util::future::join_all(submitted).await;
    assert_eq!(results, (0..5).map(Ok).collect::<Vec<_>>());
    let expected: Vec<String> = (0..5)
        .flat_map(|index| [format!("start {index}"), format!("end {index}")])
        .collect();
    assert_eq!(*log.lock().unwrap(), expected);
}

/// 提交方放弃等待不打断已经开始的工作。
#[tokio::test]
async fn an_abandoned_job_still_runs_to_completion() {
    let scope = scope();
    let serial = Serial::start(&scope.spawner(), "writes", 8);
    let done = Arc::new(Mutex::new(false));
    let (started_tx, started) = tokio::sync::oneshot::channel();
    let submitter = {
        let (serial, done) = (serial.clone(), done.clone());
        owned(async move {
            serial
                .run(async move {
                    let _ = started_tx.send(());
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    *done.lock().unwrap() = true;
                })
                .await
        })
    };
    started.await.unwrap();
    drop(submitter);
    assert_eq!(serial.run(async {}).await, Ok(()));
    assert!(*done.lock().unwrap());
}

/// 工作 panic：提交方得到同一个 panic，owner 继续执行后面的工作。
#[tokio::test]
async fn a_panicking_job_panics_its_submitter_only() {
    let scope = scope();
    let serial = Serial::start(&scope.spawner(), "writes", 8);
    let failing = {
        let serial = serial.clone();
        owned(async move {
            serial
                .run(async {
                    if true {
                        panic!("boom");
                    }
                })
                .await
        })
    };
    let Err(TaskError::Panicked(panicked)) = failing.await else {
        panic!("the submitter panics");
    };
    assert_eq!(panicked.message, "boom");
    assert_eq!(serial.run(async { 2 }).await, Ok(2));
}

/// 作用域关闭后 owner 不再执行工作，提交方得到 Stopped；owner 是常驻任务，不算被中止。
#[tokio::test]
async fn closing_the_scope_stops_the_owner() {
    let scope = scope();
    let serial = Serial::start(&scope.spawner(), "writes", 8);
    assert_eq!(
        scope.close(Duration::from_secs(5)).await,
        Vec::<String>::new()
    );
    assert_eq!(serial.run(async { 1 }).await, Err(Stopped));
}
