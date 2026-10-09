use super::Engine;
use crate::{
    contract::{RuntimeError, StorageCommitFailure},
    domain::protocol::Command,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

impl Engine {
    /// V4 `deleteSession`: closes the runtime session and records the command ACK.
    pub(super) async fn close_session(&mut self, c: &Command) -> Result<Value> {
        ensure!(
            c.payload.as_object().is_some_and(serde_json::Map::is_empty),
            "deleteSession requires an empty payload"
        );
        let id = c.session_id.as_deref().context("Session id required")?;
        ensure!(self.sessions.contains_key(id), "Session unavailable");
        let ack = self.shut_down(id, Some(c)).await?;
        Ok(ack.expect("command close returns an ACK"))
    }

    /// Legacy `session/close` (Node `closeSession`): same shutdown without an ACK,
    /// optionally conditional on the session's current persistence.
    pub(super) async fn close_runtime_session(&mut self, p: &Value) -> Result<Value> {
        let params = p
            .as_object()
            .filter(|o| {
                o.keys()
                    .all(|k| k == "sessionId" || k == "expectedPersistence")
            })
            .ok_or_else(|| RuntimeError::invalid_params("Unsupported session/close fields"))?;
        let id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| RuntimeError::invalid_params("sessionId: required"))?;
        let expected = match params.get("expectedPersistence") {
            None => None,
            Some(Value::String(v)) if v == "deferred" || v == "immediate" => Some(v.as_str()),
            Some(_) => return Err(RuntimeError::invalid_params("expectedPersistence: invalid")),
        };
        let Some(session) = self.sessions.get(id) else {
            return Err(RuntimeError::Coded {
                code: -32004,
                message: format!("Session is not active: {id}"),
            }
            .into());
        };
        // V4 草稿与 legacy deferred 首次输入前都是 deferred；legacy immediate 从创建起即 immediate。
        let immediate = session.runtime.persistence
            == Some(crate::domain::session_runtime::Persistence::Immediate);
        let current = if session.phase == crate::domain::execution::Phase::Draft && !immediate {
            "deferred"
        } else {
            "immediate"
        };
        // 连接切换与跨端首发可能并发；条件关闭在 owner 上原子判断，不依赖客户端旧快照。
        if expected.is_some_and(|expected| expected != current) {
            return Ok(json!({"closed": false}));
        }
        self.shut_down(id, None).await?;
        Ok(json!({"closed": true}))
    }

    /// Shared close path: cancels foreground and background work, releases waiters,
    /// uploads and subscriptions, reclaims history-less drafts and publishes the
    /// index removal. Persisted history stays in the store.
    async fn shut_down(&mut self, id: &str, command: Option<&Command>) -> Result<Option<Value>> {
        let s = self.sessions.get_mut(id).context("Session unavailable")?;
        s.auto_drain = false;
        if let Some(goal) = &mut s.goal {
            goal.finish_run(self.clock.now(), false, Some("paused"));
        }
        s.queued_now = None;
        self.close_topic(id);
        if let Some(active) = self.active.get(id) {
            active.cancel.cancel();
        }
        self.release_waiters(id);
        self.submissions.retain(|(session, _), _| session != id);
        self.cancel_children(id).await?;
        self.tools.cancel_session(id, None).await?;
        // TS close 会释放执行资源；必须收齐真正的终态，不能提前 ACK 后让 Shell 继续写文件。
        // 同时消费其他会话事件，避免有界事件通道阻塞取消后的终态投递。
        while self.active.contains_key(id)
            || self.sessions[id]
                .children
                .values()
                .any(zcode_cli_domain::subagent::Task::running)
            || self.sessions[id]
                .background
                .values()
                .any(|task| task.status == "running")
        {
            let event = self.event_rx.recv().await.context("Run channel closed")?;
            self.apply_event(event).await?;
        }
        self.tools.close_session(id).await?;
        let s = self.sessions.get_mut(id).unwrap();
        if let Some(context) = &mut s.shared_context {
            context.release(None);
        }
        for item in std::mem::take(&mut s.queue) {
            let key = serde_json::to_string(&(Some(id), item["sourceCommandId"].as_str()))?;
            if let Some(ack) = self.acks.get_mut(&key) {
                ack["status"] = "failed".into();
                ack["reasonCode"] = "fault.input.discardedOnClose".into();
                ack["result"] =
                    json!({"type":"inputDisposition","delivery":ack["result"]["delivery"]});
                s.pending_acks.insert(key, ack.clone());
            }
        }
        s.revision += 1;
        let ack = command.map(|c| (c.key(), c.ack("accepted", s.revision, None)));
        let workspace = s.workspace.clone();
        if s.rows.is_empty() && s.messages.is_empty() && s.shared_context.is_none() {
            self.store
                .discard_draft(&workspace, id, ack.clone())
                .await
                .context(StorageCommitFailure)?;
        } else {
            self.persist(id, ack.clone()).await?;
        }
        // 删除命令只关闭 runtime。历史保留在 Store，重开从新 epoch 冷恢复；失败不得发移除事实。
        self.uploads.0.retain(|key, _| key.1 != id);
        self.sessions.remove(id);
        self.hooks.sessions.remove(id);
        self.hooks.without.remove(id);
        self.session_access.remove(id);
        self.closed.insert(id.into());
        self.publish_index(&workspace, id, None)?;
        Ok(ack.map(|(key, ack)| {
            self.acks.insert(key, ack.clone());
            ack
        }))
    }

    /// Makes session `id` resident, from the store when needed. A request that
    /// names no workspace addresses the default one: a resident session is
    /// served as is, a stored one only loads from the default workspace.
    pub(super) async fn ensure_session(&mut self, id: &str) -> Result<()> {
        self.ensure_session_in(id, None).await
    }

    /// [`Self::ensure_session`] for a request that names `workspace` (or a
    /// child that inherits its parent's): only a session of that workspace key
    /// is accepted, resident or stored. The session keeps its stored key and path.
    pub(super) async fn ensure_session_in(
        &mut self,
        id: &str,
        workspace: Option<&crate::contract::Workspace>,
    ) -> Result<()> {
        if let Some(session) = self.sessions.get(id) {
            ensure!(
                workspace.is_none_or(|w| w.key == session.workspace),
                "Session unavailable"
            );
            self.touch_session(id);
            return Ok(());
        }
        // 未指明 workspace 的请求属于缺省 workspace：冷加载不越过 workspace 边界（同路径不同
        // identity 的会话与订阅隔离），其他 workspace 的会话要由请求或父会话指明。
        let key = workspace.map_or(self.default_workspace.as_str(), |w| w.key.as_str());
        let mut session = self
            .store
            .load_session(Some(key), id)
            .await?
            .context("Session unavailable")?;
        ensure!(
            session.context.offset <= session.messages.len(),
            "Invalid persisted session boundary"
        );
        let workspace = Self::session_workspace(&session);
        self.open_scope(&workspace).await?;
        // 修复：删除工作区锁后，启动时扫描并恢复全部未完成的文件回退事务会碰到其他进程正在
        // 进行的事务；改为会话被本进程加载（Host owner/lease 保证唯一 live runtime）时只恢复它自己的。
        self.tools
            .recover_rewind(id, session.rewind_committed.as_deref())
            .await?;
        session.validate_history()?;
        session.recover(self.clock.id(), self.clock.now());
        super::file_changes::import(&mut session, self.tools.as_ref()).await;
        self.seed_context_window(&mut session);
        super::file_changes::hydrate(&mut session, self.tools.as_ref(), None).await?;
        self.store
            .commit_receipt(&workspace.key, Some(&mut session), None)
            .await
            .context(StorageCommitFailure)?;
        let summary = (session.listed && !session.archived).then(|| session.summary());
        self.sessions.insert(id.into(), session);
        self.touch_session(id);
        self.closed.remove(id);
        self.publish_index(&workspace.key, id, summary)
    }

    pub(super) async fn conversation_query(
        &mut self,
        method: crate::contract::Method,
        p: &Value,
    ) -> Result<Value> {
        let id = p["sessionId"].as_str().context("Session id required")?;
        self.ensure_session(id).await?;
        let name = method.as_str();
        if name.contains("attachment") || name.starts_with("v4/attachment/") {
            self.attachment_query(name, p).await
        } else {
            self.query(method, p)
        }
    }
}
