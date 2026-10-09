//! 串行键的调用通道（docs/specs/rust-u2-gateway.md §2.2）。
//!
//! 每个键一条通道：队首的调用在执行，其余按登记顺序排队。登记（[`Lanes::enter`]）由连接循环在
//! 请求到达时同步完成，排队位置因此等于到达 Gateway 的顺序，不依赖任务调度，也不依赖异步锁的
//! 公平性。队首结束（[`Ticket`] 被释放：完成、失败、panic 或任务被中止）时把通道交给下一个仍在
//! 排队的调用；排队中被撤出的调用（客户端取消、连接关闭）直接离队，不会开始执行。
//!
//! ```text
//! enter(k) ─► 通道空闲：成为队首，立即开始
//!          └► 通道忙：排到队尾，等待 turn()
//! 队首的 Ticket 释放 ─► 唤醒下一个 ─► 它成为队首
//! withdraw / withdraw_connection ─► 从队中移除（发送端被丢弃，turn() 返回 false）
//! ```
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::oneshot;

/// Host 级的通道表，由 Gateway 持有，所有连接共用。
#[derive(Default)]
pub(crate) struct Lanes {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_ticket: u64,
    lanes: HashMap<String, Lane>,
}

struct Lane {
    /// 正在执行的调用。
    holder: u64,
    waiting: VecDeque<Waiter>,
}

struct Waiter {
    ticket: u64,
    connection: u64,
    admit: oneshot::Sender<()>,
}

impl Lanes {
    /// 锁只保护表本身，持有期间不 await；每次修改都是单步，中毒时数据仍然一致。
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 登记一个调用。通道空闲时它立即成为队首，否则排到队尾。
    pub(crate) fn enter(self: &Arc<Self>, key: String, connection: u64) -> Ticket {
        let mut state = self.lock();
        state.next_ticket += 1;
        let id = state.next_ticket;
        let turn = match state.lanes.get_mut(&key) {
            Some(lane) => {
                let (admit, turn) = oneshot::channel();
                lane.waiting.push_back(Waiter {
                    ticket: id,
                    connection,
                    admit,
                });
                Some(turn)
            }
            None => {
                state.lanes.insert(
                    key.clone(),
                    Lane {
                        holder: id,
                        waiting: VecDeque::new(),
                    },
                );
                None
            }
        };
        drop(state);
        Ticket {
            lanes: self.clone(),
            key,
            id,
            turn,
        }
    }

    /// 客户端取消了一个仍在排队的调用：离队，不会开始。已经开始的调用不受影响（由中止任务处理）。
    pub(crate) fn withdraw(&self, key: &str, ticket: u64) {
        if let Some(lane) = self.lock().lanes.get_mut(key) {
            lane.waiting.retain(|waiter| waiter.ticket != ticket);
        }
    }

    /// 连接关闭：它排队中的调用全部离队。正在执行的调用由中止任务释放通道。
    pub(crate) fn withdraw_connection(&self, connection: u64) {
        for lane in self.lock().lanes.values_mut() {
            lane.waiting
                .retain(|waiter| waiter.connection != connection);
        }
    }

    fn leave(&self, key: &str, ticket: u64) {
        let mut state = self.lock();
        let Some(lane) = state.lanes.get_mut(key) else {
            return;
        };
        if lane.holder != ticket {
            lane.waiting.retain(|waiter| waiter.ticket != ticket);
            return;
        }
        while let Some(next) = lane.waiting.pop_front() {
            // 接收端随 Ticket 一起释放，而 Ticket 释放时会先把自己从队中移除，所以发送不会失败；
            // 万一失败也只是跳过它。
            if next.admit.send(()).is_ok() {
                lane.holder = next.ticket;
                return;
            }
        }
        state.lanes.remove(key);
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.lock().lanes.is_empty()
    }
}

/// 一个调用在通道里的位置。释放即离开通道：队首释放时通道交给下一个，排队中释放时直接离队。
pub(crate) struct Ticket {
    lanes: Arc<Lanes>,
    key: String,
    id: u64,
    /// `None`：登记时通道空闲，已是队首。
    turn: Option<oneshot::Receiver<()>>,
}

impl Ticket {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// 轮到这个调用时返回 true；排队中被撤出（取消、连接关闭）时返回 false。
    pub(crate) async fn turn(&mut self) -> bool {
        match self.turn.take() {
            None => true,
            Some(turn) => turn.await.is_ok(),
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.lanes.leave(&self.key, self.id);
    }
}
