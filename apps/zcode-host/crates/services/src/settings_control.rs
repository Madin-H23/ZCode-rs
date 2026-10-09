//! Main ↔ Host 的设置消息（docs/specs/rust-u4-app-host.md §2.6、§4.1）。`SettingStore` 是
//! `setting.json` 的唯一写入方；本模块只把它接到控制通道，`settings` 本身不依赖控制通道。
//!
//! - `setting-write{requestId, patch}`：与 `setting` channel 的 `update` 同一条写入路径与校验；一个任务按
//!   到达顺序串行处理（Main 先后两次写入同一字段时，后到的必须后写），回
//!   `setting-write-result{requestId, ok, error?}`，写入落盘后才回 `ok: true`。
//! - `setting-changed{setting}`：任何一次成功写入之后的完整设置，不论写入方是谁。
//!
//! ```text
//! Main ─setting-write─► 队列（有界）─► 写入任务 ─► SettingStore.update_reporting
//!                                                  ├─ 写队列内：setting-write-result ─► Main
//!                                                  └─ 写队列内：setting-changed       ─► Main
//! Renderer ─setting.update─► SettingStore.update ─► setting-changed ─► Main
//! ```
//! 两条消息走同一个有序的控制出口，所以结果先于它引起的 `setting-changed`，`setting-changed` 按写入顺序。
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zcode_host_control::{ControlRouter, ControlSink, HostToMain};
use zcode_host_kit::{RpcError, log};

use crate::settings::{self, ChangeSink, SettingStore};

pub const SETTING_WRITE: &str = "setting-write";
pub const SETTING_WRITE_RESULT: &str = "setting-write-result";
pub const SETTING_CHANGED: &str = "setting-changed";
/// Main 的设置写入很少（窗口尺寸、缩放、更新字段）；队列满时直接回失败，不无界堆积。
const WRITE_QUEUE: usize = 64;

/// `SettingStore` 的变更出口：完整设置 → `setting-changed`。
pub fn change_sink(control: Arc<dyn ControlSink>) -> ChangeSink {
    Arc::new(move |setting: &Value| {
        control.send(HostToMain::Raw(
            json!({"type": SETTING_CHANGED, "setting": setting}),
        ));
    })
}

fn write_result(request_id: &str, result: &Result<(), RpcError>) -> HostToMain {
    let mut message =
        json!({"type": SETTING_WRITE_RESULT, "requestId": request_id, "ok": result.is_ok()});
    if let Err(error) = result {
        message["error"] = json!(error.message);
    }
    HostToMain::Raw(message)
}

/// 登记 `setting-write`，返回串行处理写入的任务（随控制路由一起结束）。
pub fn register_setting_writes(
    router: &mut ControlRouter,
    store: SettingStore,
    control: Arc<dyn ControlSink>,
) -> JoinHandle<()> {
    let (queue, mut requests) = mpsc::channel::<(String, Value)>(WRITE_QUEUE);
    let reject = control.clone();
    router.on(SETTING_WRITE, move |message| {
        let Some(request_id) = message["requestId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
        else {
            log::warn("setting-write without requestId ignored");
            return;
        };
        let patch = message.get("patch").cloned().unwrap_or(Value::Null);
        if let Err(error) = queue.try_send((request_id, patch)) {
            let (request_id, _) = match error {
                mpsc::error::TrySendError::Full(request)
                | mpsc::error::TrySendError::Closed(request) => request,
            };
            log::warn(format!(
                "setting-write rejected, requestId={request_id}: queue unavailable"
            ));
            reject.send(write_result(
                &request_id,
                &Err(RpcError::new("Error", "setting-write queue is full")),
            ));
        }
    });
    tokio::spawn(async move {
        while let Some((request_id, patch)) = requests.recv().await {
            let patch = match settings::parse_update_patch(&patch) {
                Ok(patch) => patch,
                Err(error) => {
                    control.send(write_result(&request_id, &Err(error)));
                    continue;
                }
            };
            let result = store
                .update_reporting(&patch, None, |result| {
                    control.send(write_result(&request_id, result));
                })
                .await;
            if let Err(error) = result {
                log::warn(format!(
                    "setting-write failed, requestId={request_id}: {}",
                    error.message
                ));
            }
        }
    })
}
