//! channel 抽象与契约 stub（rust-host-r0.md §2 应答规则）。
//!
//! 每个 channel 实现 [`ServiceChannel`]；还没实现的 channel 由 [`StubChannel`] 按契约清单应答：
//! 方法立即回 `NotImplemented`（少数成员回约定值），事件接受订阅但永不触发，绝不挂起。
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::stream::{self, Stream};
use serde::Serialize;
use serde_json::{Value, json};
use zcode_host_control::{ClientMode, WindowId};
use zcode_host_rpc::{RpcError, RpcValue};

use crate::builder::ServiceBuilder;
use crate::contract::{Contract, MemberKind};

pub const NOT_IMPLEMENTED_CODE: &str = "fault.host.notImplemented";

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Undefined,
    Value(Value),
    /// 已是 JSON 文本（CLI result、V4 帧），原样写入 Object 标签。
    RawJson(String),
    /// 顶层二进制（RPC Buffer 标签，Renderer 收到 Uint8Array）。
    Bytes(Vec<u8>),
    /// 已序列化的 rpc 值（标签 + 内容），原样写出：远端 workspace 的转发结果与事件逐字节保持
    /// 远端 server 的编码（docs/specs/rust-host-remote.md §2.2）。
    Encoded(Vec<u8>),
}

impl Reply {
    /// 序列化任意值；序列化失败（不可能出现在纯数据类型上）退化为 null。
    pub fn of<T: Serialize>(value: &T) -> Self {
        Reply::Value(serde_json::to_value(value).unwrap_or(Value::Null))
    }

    pub fn null() -> Self {
        Reply::Value(Value::Null)
    }
}

impl From<Value> for Reply {
    fn from(value: Value) -> Self {
        Reply::Value(value)
    }
}

pub type CallFuture = Pin<Box<dyn Future<Output = Result<Reply, RpcError>> + Send>>;
pub type EventStream = Pin<Box<dyn Stream<Item = Reply> + Send>>;

/// 连接级可信上下文：由签发 capability 时登记的元数据决定，客户端不能自报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallContext {
    /// 每条连接唯一（同一 attachmentId 重连也会换新值），按连接持有状态的 channel 以它为键。
    pub connection: u64,
    pub attachment_id: String,
    pub client_mode: ClientMode,
    /// 连接所属主窗口（`issue-attachment-endpoint.windowId`，rust-u4-app-host.md §2.2）。一个窗口可有
    /// 多条连接（reload 前后、远端 scope 的 attachment）；没有窗口身份的连接（手机远控、Host 内部）为 None。
    pub window_id: Option<WindowId>,
}

impl CallContext {
    /// 没有窗口身份的连接上下文。
    pub fn new(connection: u64, attachment_id: impl Into<String>, client_mode: ClientMode) -> Self {
        Self {
            connection,
            attachment_id: attachment_id.into(),
            client_mode,
            window_id: None,
        }
    }

    pub fn with_window(mut self, window_id: Option<WindowId>) -> Self {
        self.window_id = window_id;
        self
    }
}

/// 接入层观测到的连接发送积压边沿（docs/specs/rust-host-resilience.md §2.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowState {
    Saturated,
    Drained,
}

impl FlowState {
    /// `v4/connection/flow` 的 `state` 取值。
    pub fn as_str(self) -> &'static str {
        match self {
            FlowState::Saturated => "saturated",
            FlowState::Drained => "drained",
        }
    }
}

pub trait ServiceChannel: Send + Sync {
    /// 这次调用的串行键（docs/specs/rust-u2-gateway.md §2.2）：键相同的调用在整个 Host 内按到达 Gateway
    /// 的顺序逐个执行，前一个结束（完成、失败或被取消）后下一个才开始；`None`（缺省）的调用立即开始。
    ///
    /// 在连接循环里同步调用，只能读参数，不能阻塞或产生副作用。有键的调用直到轮到它时才调用
    /// [`Self::call`]，排队中被取消的调用不会走到 `call`。
    fn serial_key(
        &self,
        _context: &CallContext,
        _method: &str,
        _args: &[RpcValue],
    ) -> Option<String> {
        None
    }
    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture;
    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError>;
    /// attachment 断开：按连接持有状态的 channel（如 V4 订阅归属）在这里清理。
    fn connection_closed(&self, _context: &CallContext) {}
    /// 连接发送积压越过水位：按连接持有下游订阅的 channel（如 V4 连接 scope）在这里转发。
    fn connection_flow(&self, _context: &CallContext, _state: FlowState) {}
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

    /// 用 builder 覆盖同名 channel；builder 没注册的成员仍由原 channel（契约 stub）应答。
    pub fn implement(&mut self, builder: ServiceBuilder) {
        let fallback = self.channels.get(builder.channel()).cloned();
        let name = builder.channel().to_owned();
        self.channels.insert(name, builder.build(fallback));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ServiceChannel>> {
        self.channels.get(name).cloned()
    }

    pub fn connection_closed(&self, context: &CallContext) {
        for channel in self.channels.values() {
            channel.connection_closed(context);
        }
    }

    pub fn connection_flow(&self, context: &CallContext, state: FlowState) {
        for channel in self.channels.values() {
            channel.connection_flow(context, state);
        }
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
