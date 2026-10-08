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

/// 远端 attachment 上的 `usage-stats`（remoteWorkspaceServiceCollection.ts 的 createUsageStatsService）：
/// App 用量统计读远端 Agent 数据库（`zcode-agent.getAppUsageStats`），其余（套餐额度等）用本机账号。
pub struct RemoteUsageStatsChannel {
    local: Arc<dyn ServiceChannel>,
    client: RpcClient,
}

impl RemoteUsageStatsChannel {
    pub fn new(local: Arc<dyn ServiceChannel>, client: RpcClient) -> Arc<Self> {
        Arc::new(Self { local, client })
    }
}

impl ServiceChannel for RemoteUsageStatsChannel {
    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        if method != "getAppUsageSnapshot" {
            return self.local.call(context, method, args);
        }
        let request = args
            .into_iter()
            .next()
            .map(RpcValue::into_json)
            .unwrap_or(serde_json::Value::Null);
        // `{range: request.range, timeZone: request.timeZone}`：缺省字段不出现在 JSON 里。
        let mut params = serde_json::Map::new();
        for key in ["range", "timeZone"] {
            if let Some(value) = request.get(key).filter(|value| !value.is_null()) {
                params.insert(key.to_owned(), value.clone());
            }
        }
        let call = self.client.call_encoded(
            "zcode-agent",
            "getAppUsageStats",
            vec![RpcValue::Json(serde_json::Value::Object(params))],
        );
        Box::pin(async move { call.await.map(Reply::Encoded) })
    }

    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        self.local.listen(context, event, arg)
    }

    fn connection_closed(&self, context: &CallContext) {
        self.local.connection_closed(context);
    }
}

/// 远端 attachment 上的 `conversation-share`：Node 为远端集合构造的 ConversationShareService 没有
/// session 服务，导入以 `feature_disabled`（`Conversation share import is unavailable`）拒绝；能力发现、
/// 预览、续接与已导入副本走本机实现。replayable 连接仍由本机实现先按 desktop-only 拒绝。
pub struct RemoteConversationShareChannel {
    local: Arc<dyn ServiceChannel>,
}

impl RemoteConversationShareChannel {
    pub fn new(local: Arc<dyn ServiceChannel>) -> Arc<Self> {
        Arc::new(Self { local })
    }
}

impl ServiceChannel for RemoteConversationShareChannel {
    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        if method == "importShare"
            && context.client_mode == zcode_host_control::ClientMode::DesktopContinuous
        {
            let error = crate::conversation_share::errors::ServiceError::new(
                "feature_disabled",
                "Conversation share import is unavailable",
            )
            .to_rpc();
            return Box::pin(async move { Err(error) });
        }
        self.local.call(context, method, args)
    }

    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        self.local.listen(context, event, arg)
    }

    fn connection_closed(&self, context: &CallContext) {
        self.local.connection_closed(context);
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
    if let Some(usage) = local.get("usage-stats") {
        registry.register(
            "usage-stats",
            RemoteUsageStatsChannel::new(usage, client.clone()),
        );
    }
    if let Some(share) = local.get("conversation-share") {
        registry.register(
            "conversation-share",
            RemoteConversationShareChannel::new(share),
        );
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
