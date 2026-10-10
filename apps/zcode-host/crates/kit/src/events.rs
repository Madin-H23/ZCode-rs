//! 进程内事件分发：每个订阅者一条有界队列，发送方不会因为某个订阅者读得慢而丢掉别人的事件，也不会像
//! broadcast 那样在订阅者落后时静默丢消息（V4 帧丢失只能靠 resync 补，不能在这里制造缺口）。
//! 订阅者断开后自动清理。
//!
//! 队列满了以后怎么办由每个 hub 在构造时写明（[`Overflow`]，docs/specs/rust-u2-task-scope.md §6.2）：
//! 同步的 [`EventHub::emit`] 不能等待，按策略结束落后的订阅者或合并信号；能等待的发送方用
//! [`EventHub::emit_waiting`]，等每个订阅者都有空位（背压）。
use std::sync::{Arc, Mutex};

use futures_util::stream;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::channel::{EventStream, Reply};
use crate::log;

/// 把 JSON 值事件流包装成 channel 的事件流。
pub fn replies(events: impl Stream<Item = Value> + Send + 'static) -> EventStream {
    Box::pin(events.map(Reply::Value))
}

/// 某个订阅者的队列满了（落后 `capacity` 条）时 [`EventHub::emit`] 的做法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// 结束这个订阅者的事件流：已入队的照常送达，之后流结束，记一条 warn。用于不能丢的事件：落后
    /// `capacity` 条说明订阅者已经卡住，悄悄丢一条会留下看不见的缺口，无限缓存会拖垮整个 Host；
    /// 流结束是订阅方看得见的失败（Renderer 的订阅随之结束，重新订阅时取快照）。
    CloseSubscriber,
    /// 丢弃新事件、保留订阅者：只用于表示“有变化”的信号。队列里已经有待处理的一条，订阅者处理它时
    /// 会去读最新状态，多出的一条没有信息量。
    Coalesce,
}

type Filter<T> = Box<dyn Fn(&T) -> bool + Send + Sync>;

struct Subscriber<T> {
    filter: Option<Filter<T>>,
    sender: mpsc::Sender<T>,
}

impl<T> Subscriber<T> {
    fn wants(&self, value: &T) -> bool {
        self.filter.as_ref().is_none_or(|filter| filter(value))
    }
}

struct Shared<T> {
    /// 用于日志：哪个 hub 结束了订阅者。
    name: &'static str,
    capacity: usize,
    overflow: Overflow,
    subscribers: Mutex<Vec<Subscriber<T>>>,
}

pub struct EventHub<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for EventHub<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T: Clone + Send + 'static> EventHub<T> {
    /// `capacity` 是每个订阅者的队列容量（条），`overflow` 是它满了以后的做法；两者都要在构造处写明理由。
    pub fn new(name: &'static str, capacity: usize, overflow: Overflow) -> Self {
        Self {
            shared: Arc::new(Shared {
                name,
                capacity: capacity.max(1),
                overflow,
                subscribers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 同步发出：不等待。某个订阅者的队列满了时按构造时的 [`Overflow`] 处理，其他订阅者不受影响。
    pub fn emit(&self, value: T) {
        let mut closed = 0usize;
        {
            let mut subscribers = self.lock();
            subscribers.retain(|subscriber| {
                if !subscriber.wants(&value) {
                    return !subscriber.sender.is_closed();
                }
                match subscriber.sender.try_send(value.clone()) {
                    Ok(()) => true,
                    Err(TrySendError::Closed(_)) => false,
                    Err(TrySendError::Full(_)) => match self.shared.overflow {
                        Overflow::CloseSubscriber => {
                            closed += 1;
                            false
                        }
                        Overflow::Coalesce => true,
                    },
                }
            });
        }
        if closed > 0 {
            log::warn(format!(
                "event hub {}: {closed} subscriber(s) fell {} events behind; closing their streams",
                self.shared.name, self.shared.capacity
            ));
        }
    }

    /// 发出并等待：每个想要这条事件的订阅者都有空位时才返回（背压）。只给能停下来等的发送方用（例如
    /// PTY 读循环：等待期间不读，PTY 缓冲写满后子进程写阻塞），且订阅者的处理不能反过来等这个发送方。
    pub async fn emit_waiting(&self, value: T) {
        let senders: Vec<mpsc::Sender<T>> = {
            let mut subscribers = self.lock();
            subscribers.retain(|subscriber| !subscriber.sender.is_closed());
            subscribers
                .iter()
                .filter(|subscriber| subscriber.wants(&value))
                .map(|subscriber| subscriber.sender.clone())
                .collect()
        };
        for sender in senders {
            // 订阅者在等待期间断开：发送失败，下次发出时清理。
            let _ = sender.send(value.clone()).await;
        }
    }

    pub fn subscribe(&self) -> impl Stream<Item = T> + Send + 'static {
        self.subscribe_inner(None)
    }

    pub fn subscribe_filtered(
        &self,
        filter: impl Fn(&T) -> bool + Send + Sync + 'static,
    ) -> impl Stream<Item = T> + Send + 'static {
        self.subscribe_inner(Some(Box::new(filter)))
    }

    /// 结束现有订阅者的事件流（相当于 TS `Emitter.dispose()`）：订阅方随后收到流结束。
    pub fn close(&self) {
        self.lock().clear();
    }

    pub fn subscriber_count(&self) -> usize {
        let mut subscribers = self.lock();
        subscribers.retain(|subscriber| !subscriber.sender.is_closed());
        subscribers.len()
    }

    fn subscribe_inner(&self, filter: Option<Filter<T>>) -> impl Stream<Item = T> + Send + 'static {
        let (sender, receiver) = mpsc::channel(self.shared.capacity);
        self.lock().push(Subscriber { filter, sender });
        stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|value| (value, receiver))
        })
    }

    /// 锁只保护订阅者表，持有期间不 await。
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Subscriber<T>>> {
        self.shared
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
