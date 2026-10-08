//! 按方法注册 service 实现；未注册的成员交给回退 channel（通常是契约 stub）。
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use zcode_host_rpc::{RpcError, RpcValue};

use crate::args::Args;
use crate::channel::{CallContext, CallFuture, EventStream, Reply, ServiceChannel};

type MethodFn = Arc<dyn Fn(CallContext, Args) -> CallFuture + Send + Sync>;
type ClosedFn = Arc<dyn Fn(&CallContext) + Send + Sync>;
type ListenFn = Arc<dyn Fn(&CallContext, Value) -> Result<EventStream, RpcError> + Send + Sync>;

pub struct ServiceBuilder {
    channel: String,
    methods: HashMap<String, MethodFn>,
    events: HashMap<String, ListenFn>,
    closed: Option<ClosedFn>,
}

impl ServiceBuilder {
    pub fn new(channel: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            methods: HashMap::new(),
            events: HashMap::new(),
            closed: None,
        }
    }

    pub fn on_connection_closed(
        mut self,
        handler: impl Fn(&CallContext) + Send + Sync + 'static,
    ) -> Self {
        self.closed = Some(Arc::new(handler));
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
            fallback,
        })
    }
}

struct BuiltService {
    channel: String,
    methods: HashMap<String, MethodFn>,
    events: HashMap<String, ListenFn>,
    closed: Option<ClosedFn>,
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
    }
}
