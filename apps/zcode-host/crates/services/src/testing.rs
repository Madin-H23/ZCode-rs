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
