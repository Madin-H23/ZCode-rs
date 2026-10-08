//! 进程内事件分发：每个订阅者一条无界队列，发送方从不阻塞，也不会像 broadcast 那样在订阅者
//! 落后时静默丢消息（V4 帧丢失只能靠 resync 补，不能在这里制造缺口）。订阅者断开后自动清理。
use std::sync::{Arc, Mutex};

use futures_util::stream;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::channel::{EventStream, Reply};

/// 把 JSON 值事件流包装成 channel 的事件流。
pub fn replies(events: impl Stream<Item = Value> + Send + 'static) -> EventStream {
    Box::pin(events.map(Reply::Value))
}

type Filter<T> = Box<dyn Fn(&T) -> bool + Send + Sync>;

struct Subscriber<T> {
    filter: Option<Filter<T>>,
    sender: mpsc::UnboundedSender<T>,
}

pub struct EventHub<T> {
    subscribers: Arc<Mutex<Vec<Subscriber<T>>>>,
}

impl<T> Clone for EventHub<T> {
    fn clone(&self) -> Self {
        Self {
            subscribers: self.subscribers.clone(),
        }
    }
}

impl<T> Default for EventHub<T> {
    fn default() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl<T: Clone + Send + 'static> EventHub<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn emit(&self, value: T) {
        let mut subscribers = self.lock();
        subscribers.retain(|subscriber| {
            if subscriber
                .filter
                .as_ref()
                .is_some_and(|filter| !filter(&value))
            {
                return !subscriber.sender.is_closed();
            }
            subscriber.sender.send(value.clone()).is_ok()
        });
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

    pub fn subscriber_count(&self) -> usize {
        let mut subscribers = self.lock();
        subscribers.retain(|subscriber| !subscriber.sender.is_closed());
        subscribers.len()
    }

    fn subscribe_inner(&self, filter: Option<Filter<T>>) -> impl Stream<Item = T> + Send + 'static {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.lock().push(Subscriber { filter, sender });
        stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|value| (value, receiver))
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Subscriber<T>>> {
        self.subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn delivers_to_matching_subscribers_and_drops_closed_ones() {
        let hub = EventHub::<u32>::new();
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
}
