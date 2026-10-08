//! scoped channel 表（`remoteWorkspaceServiceCollection.ts` + `exposeServicesOnMessagePort`）：
//! 远端 attachment 上，工作区相关 channel 转发到远端 `zcode-server`，本机设置、账号与窗口级
//! controller 用本地 Host 的同名 channel（rust-host-remote.md §2.5）。
use std::sync::Arc;

use futures_util::StreamExt;
use zcode_host_kit::{
    CallContext, CallFuture, ChannelRegistry, EventStream, Reply, RpcError, RpcValue,
    ServiceChannel,
};
use zcode_host_rpc::RpcClient;

/// 原样转发到远端的 channel。
pub const FORWARDED_CHANNELS: &[&str] = &[
    "file",
    "git",
    "git-checkpoint",
    "system",
    "terminal",
    "file-watcher",
    "model-selection",
    "provider-settings",
    "skills",
    "skill-sync",
    "mcp-sync",
    "plugin-sync",
    "plugins",
    "plugin-management",
    "commands",
    "hooks",
];

/// 用本地 Host 同名 channel 的 channel：本机设置、凭据、账号、窗口级 controller。
pub const LOCAL_CHANNELS: &[&str] = &[
    "window-controller",
    "setting",
    "credential",
    "broadcast",
    "bots",
    "oauth",
    "usage-stats",
    "coding-plan",
    "client-config",
    "client-scenes",
    "subagents",
    "memory",
    "settings-sync",
    "conversation-share",
];

/// 远端结果写回 Renderer：顶层 undefined 与二进制保持原标签，其余按 JSON（数组内 undefined 变 null）。
pub fn rpc_reply(value: RpcValue) -> Reply {
    match value {
        RpcValue::Undefined => Reply::Undefined,
        RpcValue::Bytes(bytes) => Reply::Bytes(bytes),
        other => Reply::Value(other.into_json()),
    }
}

/// 把一个 channel 的全部成员转发到远端。
pub struct ForwardChannel {
    client: RpcClient,
    channel: String,
}

impl ForwardChannel {
    pub fn new(client: RpcClient, channel: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            client,
            channel: channel.into(),
        })
    }
}

impl ServiceChannel for ForwardChannel {
    fn call(&self, _context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        let call = self.client.call_encoded(&self.channel, method, args);
        Box::pin(async move { call.await.map(Reply::Encoded) })
    }

    fn listen(
        &self,
        _context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        Ok(Box::pin(
            self.client
                .listen_encoded(&self.channel, event, arg)
                .map(Reply::Encoded),
        ))
    }
}

/// 组装一条远端 attachment 的 channel 表：先转发与本地 channel，再由调用方覆盖包装过的 channel。
pub fn base_registry(client: &RpcClient, local: &ChannelRegistry) -> ChannelRegistry {
    let mut registry = ChannelRegistry::empty();
    for channel in FORWARDED_CHANNELS {
        registry.register(*channel, ForwardChannel::new(client.clone(), *channel));
    }
    for channel in LOCAL_CHANNELS {
        if let Some(local) = local.get(channel) {
            registry.register(*channel, local);
        }
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replies_keep_top_level_tags() {
        assert_eq!(rpc_reply(RpcValue::Undefined), Reply::Undefined);
        assert_eq!(rpc_reply(RpcValue::Bytes(vec![1])), Reply::Bytes(vec![1]));
        assert_eq!(rpc_reply(RpcValue::Int(3)), Reply::Value(json!(3)));
        assert_eq!(
            rpc_reply(RpcValue::Array(vec![
                RpcValue::Undefined,
                RpcValue::String("a".into())
            ])),
            Reply::Value(json!([null, "a"]))
        );
    }

    #[test]
    fn channel_sets_do_not_overlap() {
        for channel in FORWARDED_CHANNELS {
            assert!(!LOCAL_CHANNELS.contains(channel), "{channel}");
        }
    }
}
