//! CLI 进程生命周期与进程诊断上报 Main（docs/specs/rust-host-agent.md §2.10）：
//! runtime 已把事件构造成 `agent-process-*` 控制消息，这里按到达顺序写给 Main。
//! Main 退出或通道关闭时只丢当前事件，不影响 Agent 生命周期。
use futures_util::StreamExt;

use super::extensions::Extensions;
use crate::main_bridge::MainBridge;

pub fn forward(extensions: &Extensions, main: MainBridge) {
    let mut events = Box::pin(extensions.process_events().subscribe());
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            main.post(event.message.clone());
        }
    });
}
