//! 连接级流控转发（docs/specs/rust-host-resilience.md §2.3，对应 zcodeAgentConnectionScope.ts 的
//! `setTransportFlowState`）。
//!
//! 每条 RPC 连接一个串行 worker：接入层报告的 `saturated` / `drained` 与连接关闭的 `closed` 按发生
//! 顺序处理，`closed` 之后不再发送任何状态。每个 workspace 只在状态变化时发 `v4/connection/flow`；
//! 连接处于 `saturated` 时新用到的 workspace 立即补发 `saturated`。
use std::collections::HashMap;
use std::future::Future;

use tokio::sync::{mpsc, oneshot};

use super::runtime::WorkspaceTarget;

pub const SATURATED: &str = "saturated";
pub const DRAINED: &str = "drained";
pub const CLOSED: &str = "closed";

enum Command {
    State(&'static str),
    /// 连接新用到一个 workspace。
    Touch(WorkspaceTarget),
    Close(Vec<WorkspaceTarget>, oneshot::Sender<()>),
}

/// 串行 worker 的句柄；drop 后 worker 随之结束。
pub(crate) struct ConnectionFlow {
    queue: Option<mpsc::UnboundedSender<Command>>,
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

    async fn handle(&mut self, command: Command) {
        match command {
            Command::State(state) => {
                if self.closed || state == self.current {
                    return;
                }
                self.current = state;
                for target in (self.targets)() {
                    self.forward(target, state).await;
                }
            }
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
    /// `targets` 返回连接当前用过的 workspace；`send` 发一条 `v4/connection/flow`，返回是否送达。
    /// 不在 tokio runtime 里时（只可能出现在同步单元测试中）不启动 worker，状态一律丢弃。
    pub(crate) fn spawn<T, S, F>(targets: T, send: S) -> Self
    where
        T: Fn() -> Vec<WorkspaceTarget> + Send + 'static,
        S: Fn(WorkspaceTarget, &'static str) -> F + Send + 'static,
        F: Future<Output = bool> + Send,
    {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Self { queue: None };
        };
        let (queue, mut commands) = mpsc::unbounded_channel();
        let mut worker = Worker {
            targets,
            send,
            current: DRAINED,
            closed: false,
            forwarded: HashMap::new(),
        };
        handle.spawn(async move {
            while let Some(command) = commands.recv().await {
                worker.handle(command).await;
            }
        });
        Self { queue: Some(queue) }
    }

    fn push(&self, command: Command) {
        if let Some(queue) = &self.queue {
            let _ = queue.send(command);
        }
    }

    pub(crate) fn set_state(&self, state: &'static str) {
        self.push(Command::State(state));
    }

    pub(crate) fn touch(&self, target: WorkspaceTarget) {
        self.push(Command::Touch(target));
    }

    /// 排在已入队的边沿之后发 `closed`，发完才返回。
    pub(crate) async fn close(&self, targets: Vec<WorkspaceTarget>) {
        let (done, finished) = oneshot::channel();
        self.push(Command::Close(targets, done));
        let _ = finished.await;
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
}
