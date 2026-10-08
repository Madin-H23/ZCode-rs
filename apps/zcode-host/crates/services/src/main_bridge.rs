//! Host → Main 请求与结果关联。Main 的结果消息按 `requestId` 回到对应等待者；
//! 超时由调用方决定（对应 TS 版各自的超时语义），迟到的结果交给 `on_late` 处理。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;
use zcode_host_control::{ControlSink, HostToMain};

type LateHandler = Box<dyn FnOnce(Value) + Send>;

enum Pending {
    Waiting(oneshot::Sender<Value>),
    /// 等待方已超时；结果到达时交给它（例如释放迟到的 claim）。
    TimedOut(LateHandler),
}

#[derive(Clone)]
pub struct MainBridge {
    control: Arc<dyn ControlSink>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
}

impl MainBridge {
    pub fn new(control: Arc<dyn ControlSink>) -> Self {
        Self {
            control,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 发一条不需要结果的消息。
    pub fn post(&self, message: Value) {
        self.control.send(HostToMain::Raw(message));
    }

    /// 发请求并等待 `requestId` 相同的结果；超时返回 None，之后到达的结果交给 `on_late`。
    pub async fn request(
        &self,
        request_id: String,
        message: Value,
        timeout: Duration,
        on_late: impl FnOnce(Value) + Send + 'static,
    ) -> Option<Value> {
        let (sender, receiver) = oneshot::channel();
        self.lock()
            .insert(request_id.clone(), Pending::Waiting(sender));
        self.post(message);
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => Some(result),
            _ => {
                let mut pending = self.lock();
                if let Some(Pending::Waiting(_)) = pending.remove(&request_id) {
                    pending.insert(request_id, Pending::TimedOut(Box::new(on_late)));
                }
                None
            }
        }
    }

    /// Main 的结果消息入口；返回是否有对应的请求。
    pub fn resolve(&self, request_id: &str, result: Value) -> bool {
        let entry = self.lock().remove(request_id);
        match entry {
            Some(Pending::Waiting(sender)) => {
                let _ = sender.send(result);
                true
            }
            Some(Pending::TimedOut(on_late)) => {
                on_late(result);
                true
            }
            None => false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 随机请求 id（十六进制）。
pub fn random_id() -> String {
    let mut buffer = [0u8; 16];
    // 熵源不可用时退化为时间戳；请求 id 只用于关联，不承担安全职责。
    if getrandom::fill(&mut buffer).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        return format!("{nanos:x}");
    }
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}
