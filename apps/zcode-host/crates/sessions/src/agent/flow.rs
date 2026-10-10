//! 连接级流控转发（docs/specs/rust-host-resilience.md §2.3，对应 zcodeAgentConnectionScope.ts 的
//! `setTransportFlowState`）。
//!
//! 每条 RPC 连接一个串行 worker：接入层报告的 `saturated` / `drained` 与连接关闭的 `closed` 按发生
//! 顺序处理，`closed` 之后不再发送任何状态。每个 workspace 只在状态变化时发 `v4/connection/flow`；
//! 连接处于 `saturated` 时新用到的 workspace 立即补发 `saturated`。
//!
//! 队列有界（docs/specs/rust-u2-task-scope.md §6.2），容量 256。接入层同步报告边沿与新用到的
//! workspace，不能等：队列满了以后的边沿合并为“最新状态”，worker 处理完队列再转发它（中间被覆盖的
//! 边沿成对抵消，最终状态不变）；合并期间的新边沿一律进合并，不会越过它；满了的补发（新用到的
//! workspace）丢弃并记 warn，下一次状态变化会向全部 workspace 转发。`close` 是异步的，满了等空位，
//! 之后合并的状态不再发送。没有积压时行为与无界队列相同。worker 是连接作用域里的任务。
use std::collections::HashMap;
use std::future::Future;

use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Notify, mpsc, oneshot};
use zcode_async::Spawner;
use zcode_host_kit::log;

use super::runtime::WorkspaceTarget;

pub const SATURATED: &str = "saturated";
pub const DRAINED: &str = "drained";
pub const CLOSED: &str = "closed";

const QUEUE: usize = 256;

enum Command {
    State(&'static str),
    /// 连接新用到一个 workspace。
    Touch(WorkspaceTarget),
    Close(Vec<WorkspaceTarget>, oneshot::Sender<()>),
}

/// 队列满了以后合并的最新状态；worker 在队列处理完之后取走。
#[derive(Default)]
struct Overflow {
    state: Mutex<Option<&'static str>>,
    ready: Notify,
}

/// 串行 worker 的句柄；drop 后 worker 随之结束。
pub(crate) struct ConnectionFlow {
    queue: mpsc::Sender<Command>,
    overflow: Arc<Overflow>,
}

struct Worker<T, S> {
    targets: T,
    send: S,
    current: &'static str,
    closed: bool,
    /// workspace key → 已转发给 CLI 的状态；缺失等同 drained。
    forwarded: HashMap<String, &'static str>,
}

impl<T, S, F> Worker<T, S>
where
    T: Fn() -> Vec<WorkspaceTarget>,
    S: Fn(WorkspaceTarget, &'static str) -> F,
    F: Future<Output = bool>,
{
    async fn forward(&mut self, target: WorkspaceTarget, state: &'static str) {
        let key = target.key();
        if self.forwarded.get(&key).copied().unwrap_or(DRAINED) == state {
            return;
        }
        // 没有存活进程时不记录：之后再用到这个 workspace 会经 Touch 补发。
        if (self.send)(target, state).await {
            self.forwarded.insert(key, state);
        }
    }

    async fn set_state(&mut self, state: &'static str) {
        if self.closed || state == self.current {
            return;
        }
        self.current = state;
        for target in (self.targets)() {
            self.forward(target, state).await;
        }
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::State(state) => self.set_state(state).await,
            Command::Touch(target) => {
                if !self.closed && self.current == SATURATED {
                    self.forward(target, SATURATED).await;
                }
            }
            Command::Close(targets, done) => {
                if !self.closed {
                    self.closed = true;
                    for target in targets {
                        (self.send)(target, CLOSED).await;
                    }
                }
                let _ = done.send(());
            }
        }
    }
}

impl ConnectionFlow {
    /// 在 `spawner`（连接的作用域）里启动 worker `connection-flow`。`targets` 返回连接当前用过的
    /// workspace；`send` 发一条 `v4/connection/flow`，返回是否送达。
    pub(crate) fn spawn<T, S, F>(spawner: &Spawner, targets: T, send: S) -> Self
    where
        T: Fn() -> Vec<WorkspaceTarget> + Send + 'static,
        S: Fn(WorkspaceTarget, &'static str) -> F + Send + 'static,
        F: Future<Output = bool> + Send,
    {
        let (queue, mut commands) = mpsc::channel(QUEUE);
        let overflow = Arc::new(Overflow::default());
        let merged = overflow.clone();
        let mut worker = Worker {
            targets,
            send,
            current: DRAINED,
            closed: false,
            forwarded: HashMap::new(),
        };
        drop(spawner.spawn("connection-flow", async move {
            loop {
                tokio::select! {
                    // 先处理队列：合并的状态晚于队列里的边沿。
                    biased;
                    command = commands.recv() => match command {
                        Some(command) => worker.handle(command).await,
                        None => return,
                    },
                    () = merged.ready.notified() => {
                        let state = merged.state.lock().unwrap_or_else(PoisonError::into_inner).take();
                        if let Some(state) = state {
                            worker.set_state(state).await;
                        }
                    }
                }
            }
        }));
        Self { queue, overflow }
    }

    pub(crate) fn set_state(&self, state: &'static str) {
        // 已经在合并时新边沿也进合并：不能越过合并里更早的边沿进队列。
        let mut merged = self
            .overflow
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if merged.is_some() || self.queue.try_send(Command::State(state)).is_err() {
            *merged = Some(state);
            self.overflow.ready.notify_one();
        }
    }

    pub(crate) fn touch(&self, target: WorkspaceTarget) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.queue.try_send(Command::Touch(target))
        {
            log::warn("connection flow queue is full; dropping a saturated replay for a workspace");
        }
    }

    /// 排在已报告的边沿之后发 `closed`，发完才返回。
    pub(crate) async fn close(&self, targets: Vec<WorkspaceTarget>) {
        let (done, finished) = oneshot::channel();
        if self.queue.send(Command::Close(targets, done)).await.is_ok() {
            let _ = finished.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<(String, &'static str)>>>;

    fn flow(
        targets: Arc<Mutex<Vec<WorkspaceTarget>>>,
        live: &'static [&'static str],
    ) -> (ConnectionFlow, Log) {
        let log: Log = Arc::default();
        let sent = log.clone();
        let flow = ConnectionFlow::spawn(
            &zcode_host_base::testing::spawner(),
            move || targets.lock().unwrap().clone(),
            move |target: WorkspaceTarget, state| {
                let sent = sent.clone();
                async move {
                    if !live.contains(&target.workspace_path.as_str()) {
                        return false;
                    }
                    sent.lock().unwrap().push((target.workspace_path, state));
                    true
                }
            },
        );
        (flow, log)
    }

    #[tokio::test]
    async fn edges_are_forwarded_in_order_and_deduplicated() {
        let targets = Arc::new(Mutex::new(vec![WorkspaceTarget::local("/a")]));
        let (flow, log) = flow(targets.clone(), &["/a", "/b"]);
        flow.set_state(DRAINED); // 初始即 drained，不发送
        flow.set_state(SATURATED);
        flow.set_state(SATURATED);
        // 饱和期间新用到的 workspace 补发 saturated；没有存活进程的不发。
        targets.lock().unwrap().push(WorkspaceTarget::local("/b"));
        flow.touch(WorkspaceTarget::local("/b"));
        flow.touch(WorkspaceTarget::local("/dead"));
        flow.set_state(DRAINED);
        let all = targets.lock().unwrap().clone();
        flow.close(all).await;
        // closed 之后的边沿被丢弃。
        flow.set_state(SATURATED);
        flow.close(Vec::new()).await;
        let sent = log.lock().unwrap().clone();
        assert_eq!(
            sent,
            [
                ("/a".to_owned(), SATURATED),
                ("/b".to_owned(), SATURATED),
                ("/a".to_owned(), DRAINED),
                ("/b".to_owned(), DRAINED),
                ("/a".to_owned(), CLOSED),
                ("/b".to_owned(), CLOSED),
            ]
        );
    }

    #[tokio::test]
    async fn touch_while_drained_sends_nothing() {
        let targets = Arc::new(Mutex::new(Vec::new()));
        let (flow, log) = flow(targets, &["/a"]);
        flow.touch(WorkspaceTarget::local("/a"));
        flow.close(Vec::new()).await;
        assert!(log.lock().unwrap().is_empty());
    }

    /// 队列满了以后（worker 卡在一次转发里）的边沿合并为最新状态：最终状态不丢，积压不超过队列容量，
    /// closed 仍在最后（docs/specs/rust-u2-task-scope.md §6.2）。
    #[tokio::test]
    async fn edges_beyond_the_queue_merge_into_the_latest_state() {
        let targets = Arc::new(Mutex::new(vec![WorkspaceTarget::local("/a")]));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let log: Log = Arc::default();
        let (sent, wait, listed) = (log.clone(), gate.clone(), targets.clone());
        let flow = ConnectionFlow::spawn(
            &zcode_host_base::testing::spawner(),
            move || listed.lock().unwrap().clone(),
            move |target: WorkspaceTarget, state| {
                let (sent, wait) = (sent.clone(), wait.clone());
                async move {
                    wait.acquire().await.unwrap().forget();
                    sent.lock().unwrap().push((target.workspace_path, state));
                    true
                }
            },
        );
        flow.set_state(SATURATED);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        for _ in 0..QUEUE {
            flow.set_state(DRAINED);
            flow.set_state(SATURATED);
        }
        flow.set_state(DRAINED);
        gate.add_permits(10 * QUEUE);
        // 队列里的边沿逐条转发，最后转发合并的最新状态。
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while log.lock().unwrap().last() != Some(&("/a".to_owned(), DRAINED))
                || log.lock().unwrap().len() < QUEUE
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the merged state is forwarded");
        let all = targets.lock().unwrap().clone();
        flow.close(all).await;
        let sent = log.lock().unwrap().clone();
        assert!(sent.len() <= QUEUE + 3, "{}", sent.len());
        assert_eq!(
            sent[sent.len() - 2..],
            [("/a".to_owned(), DRAINED), ("/a".to_owned(), CLOSED)]
        );
    }
}
