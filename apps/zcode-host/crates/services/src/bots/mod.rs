//! bots（docs/specs/rust-host-automation.md 第 7 节）：Bot 配置与状态、凭据、飞书 / Lark 长连接、
//! 入站命令处理与 automation 终态回推。
//!
//! 只实现飞书 / Lark 通道；Telegram、Webhook、微信的传输层与微信扫码注册仍由 stub 应答
//! `NotImplemented`（见 spec 的偏差表）。
pub mod delivery;
pub mod feishu;
pub mod inbound;
pub mod model;
pub mod runtime;
pub mod runtime_lock;
pub mod service;
pub mod store;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, StreamExt};
use serde_json::{Value, json};
use zcode_host_control::ControlRouter;
use zcode_host_kit::{ChannelRegistry, log};

use crate::agent::WorkspaceTarget;
use crate::automation::driver::{CreateSession, ResumeSession, SendText, TerminalOutcome};
use crate::automation::{Area, BotDelivery, Keepalive};
use crate::tasks::repo::RepoFilter;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// 一次运行结束时给 Bot 回推的内容：最后一段助手文本与失败原因（若有）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FinalReply {
    pub text: Option<String>,
    pub error: Option<String>,
}

/// Bot 用到的任务能力（`IZCodeTaskService` 的子集）。生产实现走 automation 区域的 TaskDriver，
/// 测试用内存实现，入站流程因此可以不启动 CLI 验证。
pub trait TaskPort: Send + Sync {
    /// `createTask`（v4 草稿）→ 新会话 id。
    fn create(
        &self,
        target: WorkspaceTarget,
        selection: Value,
        mode: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>>;
    /// `resumeTask` + `getTaskModelSelection`。
    fn resume(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Option<Value>, String>>;
    /// V4 sendText。
    fn send(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
    ) -> BoxFuture<'static, Result<(), String>>;
    fn stop(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// 该 workspace 的任务元数据（tasks-index）。
    fn list(&self, workspace_key: String) -> BoxFuture<'static, Vec<Value>>;
    fn deleted_ids(&self, workspace_key: String) -> BoxFuture<'static, Vec<String>>;
    /// 终态事件流；必须在发送输入前订阅。
    fn terminal_outcomes(&self) -> BoxStream<'static, TerminalOutcome>;
    /// 读取一次运行的最终回复（会话最后一轮的助手文本）。
    fn final_reply(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, FinalReply>;
    /// `modelSelectionService.getView`（本 Host）。
    fn model_view(&self, input: Option<Value>) -> BoxFuture<'static, Value>;
}

/// 生产实现：automation 区域的 TaskDriver 与 provider 运行时。
struct AreaPort {
    area: Arc<Area>,
}

fn reasoning_level(selection: &Value) -> Option<String> {
    selection["options"]["reasoningLevel"]
        .as_str()
        .filter(|level| !level.is_empty())
        .map(str::to_owned)
}

impl TaskPort for AreaPort {
    fn create(
        &self,
        target: WorkspaceTarget,
        selection: Value,
        mode: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>> {
        let driver = self.area.driver.clone();
        async move {
            let meta = driver
                .create_session(
                    &target,
                    CreateSession {
                        thought_level: reasoning_level(&selection),
                        model: Some(selection),
                        mode,
                        defer_persistence: true,
                        ..CreateSession::default()
                    },
                )
                .await
                .map_err(|error| error.message)?;
            Ok(meta["taskId"].as_str().unwrap_or_default().to_owned())
        }
        .boxed()
    }

    fn resume(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Option<Value>, String>> {
        let driver = self.area.driver.clone();
        let agent = self.area.agent.clone();
        async move {
            driver
                .resume_session(&target, &task_id, ResumeSession::default())
                .await
                .map_err(|error| error.message)?;
            let process = agent
                .get_ready_client(&target)
                .await
                .map_err(|error| error.message)?;
            let snapshot = process
                .request_value("session/read", &json!({"sessionId": task_id}))
                .await
                .map_err(|error| error.message)?;
            let current = &snapshot["settings"]["model"]["current"];
            Ok((!current.is_null()).then(|| current.clone()))
        }
        .boxed()
    }

    fn send(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
    ) -> BoxFuture<'static, Result<(), String>> {
        let driver = self.area.driver.clone();
        async move {
            driver
                .send_text(&target, &task_id, input)
                .await
                .map(|_| ())
                .map_err(|error| error.message)
        }
        .boxed()
    }

    fn stop(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<(), String>> {
        let driver = self.area.driver.clone();
        async move {
            driver
                .stop(&target, &task_id)
                .await
                .map_err(|error| error.message)
        }
        .boxed()
    }

    fn list(&self, workspace_key: String) -> BoxFuture<'static, Vec<Value>> {
        let repo = self.area.tasks.repo().clone();
        async move {
            repo.list(RepoFilter {
                workspace_key: Some(workspace_key),
                ..RepoFilter::default()
            })
            .await
            .unwrap_or_default()
        }
        .boxed()
    }

    fn deleted_ids(&self, workspace_key: String) -> BoxFuture<'static, Vec<String>> {
        let repo = self.area.tasks.repo().clone();
        async move { repo.deleted_ids(workspace_key).await.unwrap_or_default() }.boxed()
    }

    fn terminal_outcomes(&self) -> BoxStream<'static, TerminalOutcome> {
        self.area.driver.terminal_outcomes().boxed()
    }

    fn final_reply(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, FinalReply> {
        let agent = self.area.agent.clone();
        async move {
            let Ok(process) = agent.get_ready_client(&target).await else {
                return FinalReply::default();
            };
            let params =
                json!({"sessionId": task_id, "limit": 200, "clientMode": "desktop-continuous"});
            match process
                .request_value("v4/conversation/rowsRange", &params)
                .await
            {
                Ok(page) => delivery::final_reply_from_rows(
                    page["rows"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                ),
                Err(error) => {
                    log::warn(format!(
                        "bots read final reply failed task={task_id}: {}",
                        error.message
                    ));
                    FinalReply::default()
                }
            }
        }
        .boxed()
    }

    fn model_view(&self, input: Option<Value>) -> BoxFuture<'static, Value> {
        let providers = self.area.providers.clone();
        async move {
            let (revision, resolved) = providers.snapshot().await;
            resolved.model_selection_view(revision, input.as_ref())
        }
        .boxed()
    }
}

/// automation 的 Bot 回推入口（`watchAutomationRun`）。
struct Delivery {
    bots: Arc<service::Bots>,
}

impl BotDelivery for Delivery {
    fn watch_automation_run(
        &self,
        target: Value,
        task_id: String,
        workspace: WorkspaceTarget,
    ) -> BoxFuture<'static, Result<(), String>> {
        let bots = self.bots.clone();
        async move {
            let params = json!({
                "target": target,
                "taskId": task_id,
                "workspacePath": workspace.workspace_path,
                "workspaceIdentity": workspace.workspace_identity,
            });
            bots.watch_automation_run(&params).await;
            Ok(())
        }
        .boxed()
    }
}

pub fn install(
    area: &Arc<Area>,
    registry: &mut ChannelRegistry,
    _router: &mut ControlRouter,
) -> Option<Keepalive> {
    let environment = &area.context.environment;
    let dir = environment.app_config_dir();
    let api =
        match feishu::api::FeishuApi::new(feishu::api::Endpoints::from_vars(&environment.vars)) {
            Ok(api) => Arc::new(api),
            Err(error) => {
                log::error(format!("bots http client unavailable: {error:#}"));
                return None;
            }
        };
    let bots = service::Bots::new(service::BotsDeps {
        repo: store::BotsRepo::new(dir.clone()),
        // 合并 account 区域后统一走它的凭据存储：同一文件只有一个写入方，写入也会广播凭据变更。
        credentials: area.credentials.clone(),
        api,
        settings: area.settings.clone(),
        tasks: Arc::new(AreaPort { area: area.clone() }),
        // desktop-attached 远端 Host 不抢跑 Bot 长连接（同 Node runStartupBackgroundTasks）。
        background: true,
    });
    let _ = area
        .bot_delivery
        .set(Arc::new(Delivery { bots: bots.clone() }));
    registry.implement(service::channel(bots.clone()));
    bots.schedule_refresh();
    Some(Box::new(bots))
}
