//! zcode-task 生命周期、查询、配置与事件成员的 channel 注册。
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Value, json};
use zcode_host_kit::{Args, Reply, ServiceBuilder};

use super::context::target_of;
use super::lifecycle::{TaskLifecycle, task_key};
use super::lifecycle_members::unsupported;
use super::service::CHANNEL;

fn first(args: &Args) -> Value {
    args.raw(0).cloned().unwrap_or(Value::Null)
}

macro_rules! member {
    ($builder:expr, $name:literal, $lifecycle:expr, |$life:ident, $params:ident| $body:expr) => {{
        let lifecycle = $lifecycle.clone();
        $builder.method($name, move |_, args| {
            let $life = lifecycle.clone();
            let $params = first(&args);
            async move { $body }
        })
    }};
}

/// 按裸 taskId（字符串参数）过滤的结果事件。
fn by_task_id(arg: &Value) -> String {
    arg.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| arg["taskId"].as_str().unwrap_or_default().to_owned())
}

pub fn service(lifecycle: Arc<TaskLifecycle>) -> ServiceBuilder {
    let b = ServiceBuilder::new(CHANNEL);
    let b = member!(b, "initialize", lifecycle, |l, p| l
        .initialize(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "createTask", lifecycle, |l, p| l
        .create_task(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "sendPrompt", lifecycle, |l, p| l
        .send_prompt(&p)
        .await
        .map(|_| Reply::Undefined));
    let b = member!(b, "enqueueTaskCommand", lifecycle, |l, p| l
        .enqueue_command(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "promoteTaskCommand", lifecycle, |l, p| l
        .promote_command(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "cancelTaskCommand", lifecycle, |l, p| l
        .cancel_command(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "resumeTask", lifecycle, |l, p| l
        .resume_task(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "closeTask", lifecycle, |l, p| l
        .close_task(&p)
        .await
        .map(|_| Reply::Undefined));
    let b = member!(b, "stopGeneration", lifecycle, |l, p| l
        .stop_generation(&p)
        .await
        .map(|_| Reply::Undefined));
    let b = member!(b, "compactSession", lifecycle, |l, p| l
        .compact_session(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "goalSession", lifecycle, |l, p| l
        .goal_session(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "respondPermission", lifecycle, |l, p| l
        .respond_interaction(&p, false)
        .await
        .map(|ok| Reply::Value(json!(ok))));
    let b = member!(b, "respondElicitation", lifecycle, |l, p| l
        .respond_interaction(&p, true)
        .await
        .map(|ok| Reply::Value(json!(ok))));
    let b = member!(b, "setMode", lifecycle, |l, p| l
        .set_mode(&p)
        .await
        .map(|_| Reply::Undefined));
    let b = member!(b, "setModel", lifecycle, |l, p| l
        .set_model(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "setConfigOption", lifecycle, |l, p| l
        .set_config_option(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "setAutomationSessionConfig", lifecycle, |l, p| l
        .set_automation_session_config(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "getTaskConfigOptions", lifecycle, |l, p| l
        .get_task_config_options(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "getTaskModelSelection", lifecycle, |l, p| l
        .get_task_model_selection(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "getTaskMeta", lifecycle, |l, p| l
        .get_task_meta(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "getTaskTokenUsage", lifecycle, |l, p| l
        .get_task_token_usage(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "getModelTrajectory", lifecycle, |l, p| l
        .get_model_trajectory(&p)
        .await
        .map(Reply::Value));
    let b = member!(b, "getTaskNativeSessionLogFile", lifecycle, |l, _p| Ok(
        Reply::Value(l.get_native_session_log_file().await)
    ));
    let b = b
        .method("getTaskSessionFilePath", |_, args| {
            let params = first(&args);
            async move {
                let path = format!(
                    "{}/{}.zcode-session",
                    params["workspacePath"].as_str().unwrap_or_default(),
                    params["taskId"].as_str().unwrap_or_default()
                );
                Ok(Reply::Value(json!({"path": path, "exists": false})))
            }
        })
        // 这三个分片读取在 Node 里同样固定回 null（快照不再分片）。
        .method("getTaskSnapshotBody", |_, _| async {
            Ok(Reply::Value(Value::Null))
        })
        .method("getTaskSnapshotRef", |_, _| async {
            Ok(Reply::Value(Value::Null))
        })
        .method("getTaskSnapshotToolCallsSlice", |_, _| async {
            Ok(Reply::Value(Value::Null))
        })
        .method("branchTaskFromPrompt", |_, _| async {
            Err(unsupported("branchTaskFromPrompt"))
        })
        .method("deliverSessionMessage", |_, _| async {
            Err(unsupported("deliverSessionMessage"))
        })
        .method("sendSessionMessageDeliveryResult", |_, _| async {
            Err(unsupported("sendSessionMessageDeliveryResult"))
        });
    let (terminal, ready, stream, task_events) = (
        lifecycle.clone(),
        lifecycle.clone(),
        lifecycle.clone(),
        lifecycle,
    );
    b.event("onDynamicTaskTerminalOutcome", move |_, arg| {
        let id = by_task_id(&arg);
        let events = terminal
            .terminal_outcomes()
            .subscribe_filtered(move |event| event.0 == id)
            .map(|event| Reply::Value(event.1.clone()));
        Ok(Box::pin(events))
    })
    .event("onDynamicTaskReady", move |_, arg| {
        let id = by_task_id(&arg);
        let events = ready
            .ready_outcomes()
            .subscribe_filtered(move |event| event.0 == id)
            .map(|event| Reply::Value(event.1.clone()));
        Ok(Box::pin(events))
    })
    .event("onDynamicStreamEvent", move |_, arg| {
        let id = by_task_id(&arg);
        let events = stream
            .stream_events()
            .subscribe_filtered(move |event| event.1 == id)
            .map(|event| Reply::Value(event.2.clone()));
        Ok(Box::pin(events))
    })
    .event("onDynamicTaskEvent", move |_, arg| {
        let key = task_key(&target_of(&arg), arg["taskId"].as_str().unwrap_or_default());
        let events = task_events
            .stream_events()
            .subscribe_filtered(move |event| event.0 == key)
            .map(|event| Reply::Value(event.2.clone()));
        Ok(Box::pin(events))
    })
    // Node 从不触发 onError；保留一个永不产出的事件流。
    .event("onError", |_, _| {
        Ok(Box::pin(futures_util::stream::pending()))
    })
}
