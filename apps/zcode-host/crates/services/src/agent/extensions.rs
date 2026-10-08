//! AgentRuntime 的扩展点。账号、自动化、浏览器控制等服务在 `install` 时登记处理器，
//! 不直接修改 runtime 与 channel 的代码：
//! - 反向请求：按方法名登记 CLI → Host 请求的处理器（优先于内置默认应答）；
//! - spawn 环境：启动 CLI 前追加环境变量（在设置页网络变量之后、命令自带变量之前）；
//! - 使用前钩子：会话被订阅、新建、读取 presentation 之前执行（例如同步 Account Config）；
//! - 通知总线：三个 lane 的 CLI 通知（V4 帧除外）与可观察的反向请求，按到达顺序广播；
//! - 进程事件：CLI 进程生命周期与进程诊断，已是 Main 控制消息的形状；
//! - 功能门禁：闲时工具、动态工作流（docs/specs/rust-host-agent.md §2.7）。
//! - channel 成员：不依赖连接 scope 的 `zcode-agent` 成员（例如 automation），由
//!   `AgentChannel` 委托给登记的处理器。
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use futures_util::future::BoxFuture;
use serde_json::Value;
use zcode_host_kit::{Args, Reply, RpcError};

use zcode_host_kit::EventHub;

use super::runtime::{AgentProcess, Lane, WorkspaceTarget};

/// 一次 CLI 反向请求的上下文。
#[derive(Debug, Clone)]
pub struct ReverseContext {
    pub target: WorkspaceTarget,
    pub workspace_key: String,
    pub generation: u64,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReverseReply {
    Result(Value),
    Error {
        code: i64,
        message: String,
    },
    /// 带 `data` 的错误（Node `client.respondError({code, message, data})`，例如 zod flatten）。
    ErrorWithData {
        code: i64,
        message: String,
        data: Value,
    },
    /// 不应答（例如权限请求，等 V4 的 resolveInteraction 赢得赛跑）。
    Hold,
}

pub type ReverseHandler =
    Arc<dyn Fn(ReverseContext) -> BoxFuture<'static, ReverseReply> + Send + Sync>;
pub type EnvContributor =
    Arc<dyn Fn(WorkspaceTarget) -> BoxFuture<'static, BTreeMap<String, String>> + Send + Sync>;
pub type MemberHandler =
    Arc<dyn Fn(Args) -> BoxFuture<'static, Result<Reply, RpcError>> + Send + Sync>;

/// CLI → Host 通知的处理器（例如 `interaction/providerRuntimeHeadersCancelled`）。
pub type NotificationHook = Arc<dyn Fn(NotificationContext) + Send + Sync>;

/// 一条 CLI 通知的上下文。
#[derive(Debug, Clone)]
pub struct NotificationContext {
    pub target: WorkspaceTarget,
    pub workspace_key: String,
    pub generation: u64,
    pub method: String,
    pub params: Value,
}

/// 本地媒体路径授权（Main 规范化并登记路径后返回规范路径），见 rust-host-workspace.md §2.3。
pub type MediaPathAuthorizer =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<String, RpcError>> + Send + Sync>;
pub type UseHook = Arc<
    dyn Fn(Arc<AgentProcess>, UseReason) -> BoxFuture<'static, Result<(), RpcError>> + Send + Sync,
>;

/// 使用前钩子的触发点（对应 Node `ensureAccountProviderConfigSynced` 的 reason）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseReason {
    ConversationSubscribe,
    CreateSession,
    ReadPresentation,
    SessionRead,
    /// legacy `resumeSession`（Node `session_resume`）。
    SessionResume,
    /// `generateWorkspaceText`（Node `workspace_generate_text`）。
    GenerateText,
    /// `testModelConnectivity`（Node `provider_test_model_connectivity`）。
    TestModelConnectivity,
}

/// 一条 CLI 通知（或只供观察的反向请求），带来源进程的身份。
#[derive(Debug, Clone)]
pub struct AgentNotification {
    pub target: WorkspaceTarget,
    pub workspace_key: String,
    pub generation: u64,
    pub lane: Lane,
    pub method: String,
    pub params: Value,
    /// true：CLI 以反向请求发出（权限、提问），这里只是旁路观察，应答仍由 runtime 决定。
    pub request: bool,
}

/// CLI 进程生命周期 / 诊断事件，`message` 已是 Main 控制消息（`agent-process-*`）。
#[derive(Debug, Clone)]
pub struct ProcessEvent {
    pub lane: Lane,
    pub message: Value,
}

pub type DynamicWorkflowGate =
    Arc<dyn Fn() -> BoxFuture<'static, Result<bool, String>> + Send + Sync>;

#[derive(Default)]
pub struct Extensions {
    reverse: RwLock<HashMap<String, ReverseHandler>>,
    env: RwLock<Vec<EnvContributor>>,
    use_hooks: RwLock<Vec<UseHook>>,
    notifications: EventHub<Arc<AgentNotification>>,
    process_events: EventHub<Arc<ProcessEvent>>,
    off_peak_tools: AtomicBool,
    dynamic_workflow_gate: RwLock<Option<DynamicWorkflowGate>>,
    dynamic_workflow: tokio::sync::OnceCell<bool>,
    members: RwLock<HashMap<String, MemberHandler>>,
    notification_hooks: RwLock<HashMap<String, NotificationHook>>,
    media_path_authorizer: RwLock<Option<MediaPathAuthorizer>>,
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Extensions {
    /// 同名方法后登记的覆盖先登记的。
    pub fn register_reverse(&self, method: impl Into<String>, handler: ReverseHandler) {
        write(&self.reverse).insert(method.into(), handler);
    }

    pub fn reverse(&self, method: &str) -> Option<ReverseHandler> {
        read(&self.reverse).get(method).cloned()
    }

    pub fn add_env_contributor(&self, contributor: EnvContributor) {
        write(&self.env).push(contributor);
    }

    pub fn env_contributors(&self) -> Vec<EnvContributor> {
        read(&self.env).clone()
    }

    pub fn add_use_hook(&self, hook: UseHook) {
        write(&self.use_hooks).push(hook);
    }

    pub fn use_hooks(&self) -> Vec<UseHook> {
        read(&self.use_hooks).clone()
    }

    /// 三个 lane 的 CLI 通知（V4 帧除外），按到达顺序。
    pub fn notifications(&self) -> &EventHub<Arc<AgentNotification>> {
        &self.notifications
    }

    pub fn process_events(&self) -> &EventHub<Arc<ProcessEvent>> {
        &self.process_events
    }

    /// 闲时任务服务可用时由它登记（Node：resolveOffPeakClientConfig 与 resolveOffPeakTaskService 都注入）。
    pub fn enable_off_peak_tools(&self) {
        self.off_peak_tools.store(true, Ordering::SeqCst);
    }

    /// isOffPeakToolSupported：门禁已登记且是本地 workspace。
    pub fn off_peak_tools_for(&self, target: &WorkspaceTarget) -> bool {
        self.off_peak_tools.load(Ordering::SeqCst) && target.is_local()
    }

    /// 动态工作流灰度的判定函数；只在首次需要时调用一次。
    pub fn set_dynamic_workflow_gate(&self, gate: DynamicWorkflowGate) {
        *write(&self.dynamic_workflow_gate) = Some(gate);
    }

    /// resolveDynamicWorkflowGate：Host 进程内只判定一次；失败按关闭处理且不重试。
    pub async fn dynamic_workflow_enabled(&self) -> bool {
        let gate = read(&self.dynamic_workflow_gate).clone();
        let Some(gate) = gate else {
            return false;
        };
        *self
            .dynamic_workflow
            .get_or_init(|| async move {
                match gate().await {
                    Ok(enabled) => enabled,
                    Err(error) => {
                        zcode_host_kit::log::warn(format!(
                            "dynamic workflow gate failed, treated as disabled: {error}"
                        ));
                        false
                    }
                }
            })
            .await
    }

    /// 会话创建 / 恢复请求的工具面参数（`buildSessionCreateParams` / `buildSessionResumeParams` 的
    /// `offPeakToolEnabled`、`dynamicWorkflowEnabled`）：门禁关闭时不写字段。Host 自己发起的
    /// create / resume（tasks、automation）都从这里取，docs/specs/rust-host-integration.md §2.4。
    pub async fn session_tool_flags(
        &self,
        target: &WorkspaceTarget,
    ) -> serde_json::Map<String, Value> {
        let mut flags = serde_json::Map::new();
        if self.off_peak_tools_for(target) {
            flags.insert("offPeakToolEnabled".into(), Value::Bool(true));
        }
        if self.dynamic_workflow_enabled().await {
            flags.insert("dynamicWorkflowEnabled".into(), Value::Bool(true));
        }
        flags
    }

    /// 登记 `zcode-agent` 成员；同名后登记的覆盖先登记的。连接相关的内置成员优先。
    pub fn register_member(&self, name: impl Into<String>, handler: MemberHandler) {
        write(&self.members).insert(name.into(), handler);
    }

    pub fn member(&self, name: &str) -> Option<MemberHandler> {
        read(&self.members).get(name).cloned()
    }

    /// 按方法名登记通知处理器；同名后登记的覆盖先登记的。
    pub fn register_notification(&self, method: impl Into<String>, hook: NotificationHook) {
        write(&self.notification_hooks).insert(method.into(), hook);
    }

    pub fn notification_hook(&self, method: &str) -> Option<NotificationHook> {
        read(&self.notification_hooks).get(method).cloned()
    }

    pub fn set_media_path_authorizer(&self, authorizer: MediaPathAuthorizer) {
        *write(&self.media_path_authorizer) = Some(authorizer);
    }

    pub fn media_path_authorizer(&self) -> Option<MediaPathAuthorizer> {
        read(&self.media_path_authorizer).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn later_registration_wins_and_hooks_accumulate() {
        let extensions = Extensions::default();
        let reply = |value: Value| -> ReverseHandler {
            Arc::new(move |_| {
                let value = value.clone();
                Box::pin(async move { ReverseReply::Result(value) })
            })
        };
        extensions.register_reverse("x/y", reply(json!(1)));
        extensions.register_reverse("x/y", reply(json!(2)));
        let handler = extensions.reverse("x/y").unwrap();
        let context = ReverseContext {
            target: WorkspaceTarget::local("/w"),
            workspace_key: "/w".into(),
            generation: 1,
            method: "x/y".into(),
            params: Value::Null,
        };
        assert_eq!(handler(context).await, ReverseReply::Result(json!(2)));
        assert!(extensions.reverse("missing").is_none());
        extensions.add_env_contributor(Arc::new(|_| {
            Box::pin(async { BTreeMap::from([("A".to_owned(), "1".to_owned())]) })
        }));
        assert_eq!(extensions.env_contributors().len(), 1);
    }
}
