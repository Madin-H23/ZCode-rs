//! channel 抽象与契约 stub（rust-host-r0.md §2 应答规则）。
//!
//! 每个 channel 实现 [`ServiceChannel`]；还没实现的 channel 由 [`StubChannel`] 按契约清单应答：
//! 方法立即回 `NotImplemented`（少数成员回约定值），事件接受订阅但永不触发，绝不挂起。
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::stream::{self, Stream};
use serde_json::{Value, json};
use zcode_host_control::ClientMode;
use zcode_host_rpc::{RpcError, RpcValue};

use crate::contract::{Contract, MemberKind};

pub const NOT_IMPLEMENTED_CODE: &str = "fault.host.notImplemented";

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Undefined,
    Value(Value),
    /// 已是 JSON 文本（CLI result、V4 帧），原样写入 Object 标签。
    RawJson(String),
}

pub type CallFuture = Pin<Box<dyn Future<Output = Result<Reply, RpcError>> + Send>>;
pub type EventStream = Pin<Box<dyn Stream<Item = Reply> + Send>>;

/// 连接级可信上下文：由签发 capability 时登记的元数据决定，客户端不能自报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallContext {
    pub attachment_id: String,
    pub client_mode: ClientMode,
}

pub trait ServiceChannel: Send + Sync {
    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture;
    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError>;
}

pub struct ChannelRegistry {
    channels: HashMap<String, Arc<dyn ServiceChannel>>,
}

impl ChannelRegistry {
    pub fn empty() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    /// 契约里的每个 channel 先注册为 stub；实现了的服务再用 [`Self::register`] 覆盖。
    pub fn from_contract(contract: Contract) -> Self {
        let mut registry = Self::empty();
        for entry in contract.channels {
            let values = stub_values(&entry.channel);
            let members = entry
                .members
                .into_iter()
                .map(|member| (member.name, member.kind))
                .collect();
            registry.register(
                entry.channel.clone(),
                Arc::new(StubChannel {
                    channel: entry.channel,
                    members,
                    values,
                }),
            );
        }
        registry
    }

    pub fn register(&mut self, name: impl Into<String>, channel: Arc<dyn ServiceChannel>) {
        self.channels.insert(name.into(), channel);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ServiceChannel>> {
        self.channels.get(name).cloned()
    }
}

/// 报错会弹窗或阻塞首屏的成员，stub 期间回约定值（rust-host-r0.md §4.5）。
fn stub_values(channel: &str) -> HashMap<String, Value> {
    let mut values = HashMap::new();
    if channel == "settings-sync" {
        // 报错时 useSettingsSync 会弹出带错误的首次引导对话框；handled 表示无需引导。
        values.insert(
            "getFirstRunPromptState".to_owned(),
            json!({"handled": true}),
        );
    }
    values
}

pub struct StubChannel {
    channel: String,
    members: HashMap<String, MemberKind>,
    values: HashMap<String, Value>,
}

impl ServiceChannel for StubChannel {
    fn call(&self, _context: &CallContext, method: &str, _args: Vec<RpcValue>) -> CallFuture {
        let result = match self.members.get(method) {
            Some(MemberKind::Method) => match self.values.get(method) {
                Some(value) => Ok(Reply::Value(value.clone())),
                None => Err(RpcError::new(
                    "NotImplemented",
                    format!(
                        "{}.{method} is not implemented in the Rust Host yet",
                        self.channel
                    ),
                )
                .with_code(NOT_IMPLEMENTED_CODE)),
            },
            // 与 ProxyChannel.fromService 相同的措辞，调用方按原有分支处理。
            _ => Err(RpcError::new(
                "Error",
                format!("Method not found: {method}"),
            )),
        };
        Box::pin(async move { result })
    }

    fn listen(
        &self,
        _context: &CallContext,
        event: &str,
        _arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        match self.members.get(event) {
            Some(MemberKind::Event | MemberKind::DynamicEvent) => Ok(Box::pin(stream::pending())),
            _ => Err(RpcError::new("Error", format!("Event not found: {event}"))),
        }
    }
}
