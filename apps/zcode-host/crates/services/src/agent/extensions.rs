//! AgentRuntime 的扩展点。账号、自动化、浏览器控制等服务在 `install` 时登记处理器，
//! 不直接修改 runtime 与 channel 的代码：
//! - 反向请求：按方法名登记 CLI → Host 请求的处理器（优先于内置默认应答）；
//! - spawn 环境：启动 CLI 前追加环境变量（在设置页网络变量之后、命令自带变量之前）；
//! - 使用前钩子：会话被订阅、新建、读取 presentation 之前执行（例如同步 Account Config）；
//! - channel 成员：不依赖连接 scope 的 `zcode-agent` 成员（例如 automation），由
//!   `AgentChannel` 委托给登记的处理器。
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use futures_util::future::BoxFuture;
use serde_json::Value;
use zcode_host_kit::{Args, Reply, RpcError};

use super::runtime::{AgentProcess, WorkspaceTarget};

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
    /// 不应答（例如权限请求，等 V4 的 resolveInteraction 赢得赛跑）。
    Hold,
}

pub type ReverseHandler =
    Arc<dyn Fn(ReverseContext) -> BoxFuture<'static, ReverseReply> + Send + Sync>;
pub type EnvContributor =
    Arc<dyn Fn(WorkspaceTarget) -> BoxFuture<'static, BTreeMap<String, String>> + Send + Sync>;
pub type MemberHandler =
    Arc<dyn Fn(Args) -> BoxFuture<'static, Result<Reply, RpcError>> + Send + Sync>;
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
}

#[derive(Default)]
pub struct Extensions {
    reverse: RwLock<HashMap<String, ReverseHandler>>,
    env: RwLock<Vec<EnvContributor>>,
    use_hooks: RwLock<Vec<UseHook>>,
    members: RwLock<HashMap<String, MemberHandler>>,
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

    /// 登记 `zcode-agent` 成员；同名后登记的覆盖先登记的。连接相关的内置成员优先。
    pub fn register_member(&self, name: impl Into<String>, handler: MemberHandler) {
        write(&self.members).insert(name.into(), handler);
    }

    pub fn member(&self, name: &str) -> Option<MemberHandler> {
        read(&self.members).get(name).cloned()
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
