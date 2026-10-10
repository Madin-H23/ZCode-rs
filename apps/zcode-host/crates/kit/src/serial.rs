//! 由一个 owner 任务按提交顺序逐个执行的工作（actor，docs/specs/rust-u2-task-scope.md §6.3）。
//!
//! 用来替代“进程内写队列”（`tokio::sync::Mutex<()>` 跨 `.await` 持有）：要串行的读改写不再在各个
//! 调用方的任务里持锁执行，而是交给资源（一个文件、一组状态）的 owner 任务，按到达顺序逐个执行。
//!
//! - 顺序：提交顺序即执行顺序（有界 mpsc 是 FIFO）；前一个执行完，后一个才开始。
//! - 取消：提交方放弃等待（future 被丢弃）不会打断已经开始的工作，读改写总是完整执行；还没开始的
//!   工作照常执行，只是结果没人读。需要随提交方一起取消的工作（原来在提交方任务里持锁执行、取消即
//!   在等待点放弃的处理）用 [`Serial::run_cancellable`]。
//! - panic：工作 panic 时 owner 继续执行后面的工作，提交方在自己的任务里得到同一个 panic，与原来在
//!   调用方任务里持锁执行时相同。
//! - 队列有界：满了时提交方等待（背压）。工作不能再向同一个队列提交并等待结果（与不可重入的锁一样会
//!   死锁）；提交方等待空位时不持有 owner 需要的任何东西。
//! - owner 是所在作用域的常驻任务：作用域关闭时它被丢弃，还在排队的工作不再执行，提交方得到
//!   [`Stopped`]。
use std::future::Future;
use std::pin::Pin;

use tokio::sync::{mpsc, oneshot};
use zcode_async::{Spawner, catch_panic};

type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

/// owner 任务已经结束（所在作用域已关闭），工作没有执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the owner task has stopped")
    }
}

impl std::error::Error for Stopped {}

/// 提交工作的句柄；可克隆，所有克隆共用一个 owner 任务。
#[derive(Clone)]
pub struct Serial {
    jobs: mpsc::Sender<Job>,
}

impl Serial {
    /// 在 `spawner` 的作用域里启动 owner 任务 `name`（常驻）。`capacity` 是排队工作的上限，满了时
    /// [`Serial::run`] 等待。
    pub fn start(spawner: &Spawner, name: &'static str, capacity: usize) -> Self {
        let (jobs, mut queue) = mpsc::channel::<Job>(capacity.max(1));
        drop(spawner.spawn_resident(name, async move {
            while let Some(job) = queue.recv().await {
                job.await;
            }
        }));
        Self { jobs }
    }

    /// 把 `job` 排到队尾，等 owner 任务执行完并返回结果（见模块文档的顺序、取消与 panic 语义）。
    pub async fn run<T, F>(&self, job: F) -> Result<T, Stopped>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (reply, result) = oneshot::channel();
        let job: Job = Box::pin(async move {
            let _ = reply.send(catch_panic(job).await);
        });
        self.submit(job, result).await
    }

    /// 同 [`Serial::run`]，但提交方放弃等待（future 被丢弃）时这项工作也被放弃：还没开始的不再执行，
    /// 已经开始的在当前等待点被丢弃。与在提交方自己的任务里持锁执行、提交方被取消时的效果相同，
    /// 顺序保证不变。
    pub async fn run_cancellable<T, F>(&self, job: F) -> Result<T, Stopped>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (mut reply, result) = oneshot::channel();
        let job: Job = Box::pin(async move {
            tokio::select! {
                // 先看提交方是否已经放弃：排队期间被放弃的工作一次都不轮询。
                biased;
                () = reply.closed() => {}
                output = catch_panic(job) => {
                    let _ = reply.send(output);
                }
            }
        });
        self.submit(job, result).await
    }

    async fn submit<T>(
        &self,
        job: Job,
        result: oneshot::Receiver<Result<T, zcode_async::Panicked>>,
    ) -> Result<T, Stopped> {
        self.jobs.send(job).await.map_err(|_| Stopped)?;
        match result.await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(panicked)) => std::panic::resume_unwind(Box::new(panicked.message)),
            Err(_) => Err(Stopped),
        }
    }
}

#[cfg(test)]
#[path = "serial_tests.rs"]
mod tests;
