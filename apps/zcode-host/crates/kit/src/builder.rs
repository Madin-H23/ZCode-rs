//! 按方法注册 service 实现；未注册的成员交给回退 channel（通常是契约 stub）。
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use zcode_host_rpc::{RpcError, RpcValue};

use crate::args::Args;
use crate::channel::{CallContext, CallFuture, EventStream, FlowState, Reply, ServiceChannel};

type MethodFn = Arc<dyn Fn(CallContext, Args) -> CallFuture + Send + Sync>;
type ClosedFn = Arc<dyn Fn(&CallContext) + Send + Sync>;
type FlowFn = Arc<dyn Fn(&CallContext, FlowState) + Send + Sync>;
type ListenFn = Arc<dyn Fn(&CallContext, Value) -> Result<EventStream, RpcError> + Send + Sync>;

pub struct ServiceBuilder {
    channel: String,
    methods: HashMap<String, MethodFn>,
    events: HashMap<String, ListenFn>,
    closed: Option<ClosedFn>,
    flow: Option<FlowFn>,
}

impl ServiceBuilder {
    pub fn new(channel: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            methods: HashMap::new(),
            events: HashMap::new(),
            closed: None,
            flow: None,
        }
    }

    pub fn on_connection_closed(
        mut self,
        handler: impl Fn(&CallContext) + Send + Sync + 'static,
    ) -> Self {
        self.closed = Some(Arc::new(handler));
        self
    }

    /// 连接出站积压的水位边沿（docs/specs/rust-u2-gateway.md §2.3）。在连接循环里同步调用，不能阻塞。
    pub fn on_connection_flow(
        mut self,
        handler: impl Fn(&CallContext, FlowState) + Send + Sync + 'static,
    ) -> Self {
        self.flow = Some(Arc::new(handler));
        self
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn method<F, Fut>(mut self, name: &str, handler: F) -> Self
    where
        F: Fn(CallContext, Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Reply, RpcError>> + Send + 'static,
    {
        self.methods.insert(
            name.to_owned(),
            Arc::new(move |context, args| Box::pin(handler(context, args))),
        );
        self
    }

    /// 事件订阅；固定事件忽略 `arg`，`onDynamicXxx` 用它过滤。
    pub fn event<F>(mut self, name: &str, listen: F) -> Self
    where
        F: Fn(&CallContext, Value) -> Result<EventStream, RpcError> + Send + Sync + 'static,
    {
        self.events.insert(name.to_owned(), Arc::new(listen));
        self
    }

    pub fn build(self, fallback: Option<Arc<dyn ServiceChannel>>) -> Arc<dyn ServiceChannel> {
        Arc::new(BuiltService {
            channel: self.channel,
            methods: self.methods,
            events: self.events,
            closed: self.closed,
            flow: self.flow,
            fallback,
        })
    }
}

struct BuiltService {
    channel: String,
    methods: HashMap<String, MethodFn>,
    events: HashMap<String, ListenFn>,
    closed: Option<ClosedFn>,
    flow: Option<FlowFn>,
    fallback: Option<Arc<dyn ServiceChannel>>,
}

impl ServiceChannel for BuiltService {
    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        if let Some(handler) = self.methods.get(method) {
            return handler(context.clone(), Args::from_rpc(args));
        }
        match &self.fallback {
            Some(fallback) => fallback.call(context, method, args),
            None => {
                let error = RpcError::new("Error", format!("Method not found: {method}"));
                Box::pin(async move { Err(error) })
            }
        }
    }

    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        if let Some(listen) = self.events.get(event) {
            return listen(context, arg.into_json());
        }
        match &self.fallback {
            Some(fallback) => fallback.listen(context, event, arg),
            None => Err(RpcError::new(
                "Error",
                format!("Event not found: {}.{event}", self.channel),
            )),
        }
    }

    fn connection_closed(&self, context: &CallContext) {
        if let Some(closed) = &self.closed {
            closed(context);
        }
        // 叠在其他实现之上时（同一 channel 分多处注册），下层按连接持有的状态也要清理。
        if let Some(fallback) = &self.fallback {
            fallback.connection_closed(context);
        }
    }

    /// 先交给自己登记的处理（例如终端暂停读 PTY），再交给被覆盖的原 channel（例如包在 zcode-agent 外的成员）。
    fn connection_flow(&self, context: &CallContext, state: FlowState) {
        if let Some(flow) = &self.flow {
            flow(context, state);
        }
        if let Some(fallback) = &self.fallback {
            fallback.connection_flow(context, state);
        }
    }
}
