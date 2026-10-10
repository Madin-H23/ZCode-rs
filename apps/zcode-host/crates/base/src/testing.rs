//! 单元测试共用：像 Renderer 一样经 channel 调用方法、订阅事件。
use std::sync::Arc;

use serde_json::Value;
use zcode_host_control::ClientMode;
use zcode_host_kit::{
    CallContext, EventStream, Reply, RpcError, RpcValue, ServiceBuilder, ServiceChannel,
};

pub fn channel(builder: ServiceBuilder) -> Arc<dyn ServiceChannel> {
    builder.build(None)
}

fn context() -> CallContext {
    CallContext {
        connection: 1,
        attachment_id: "test".to_owned(),
        client_mode: ClientMode::DesktopContinuous,
        window_id: None,
    }
}

/// 以单个对象参数调用；`undefined` 回成 JSON null。
pub async fn call(
    channel: &Arc<dyn ServiceChannel>,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    match channel
        .call(&context(), method, vec![RpcValue::Json(params)])
        .await?
    {
        Reply::Value(value) => Ok(value),
        Reply::Undefined => Ok(Value::Null),
        Reply::RawJson(raw) => Ok(serde_json::from_str(&raw).unwrap_or(Value::Null)),
        Reply::Bytes(bytes) => Ok(Value::from(bytes)),
        Reply::Encoded(body) => Ok(zcode_host_rpc::Reader::new(&body)
            .value()
            .map(RpcValue::into_json)
            .unwrap_or(Value::Null)),
    }
}

pub fn listen(
    channel: &Arc<dyn ServiceChannel>,
    event: &str,
    arg: &str,
) -> Result<EventStream, RpcError> {
    channel.listen(&context(), event, RpcValue::String(arg.to_owned()))
}

pub fn reply_value(reply: Reply) -> Value {
    match reply {
        Reply::Value(value) => value,
        _ => Value::Null,
    }
}

/// 测试里 service 的任务作用域：每次调用新建一个根作用域，随测试进程存在（不关闭、不释放），构造
/// service 的测试不用再持有它。任务 panic 记进日志，等待方照常得到错误。
pub fn spawner() -> zcode_async::Spawner {
    let scope = zcode_async::TaskScope::root("test", |panic| {
        zcode_host_kit::log::error(format!(
            "task panicked: scope={} task={}: {}",
            panic.scope, panic.task, panic.message
        ));
    });
    let spawner = scope.spawner();
    std::mem::forget(scope);
    spawner
}
