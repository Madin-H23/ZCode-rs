//! Session side of `@plugin` references (spec rust-m10-plugins §3.9): the
//! frozen reference catalog and the model-only notice a run commits.
use super::Engine;
use crate::contract::{Event, RunEvent};
use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

impl Engine {
    /// Host 对反向请求的应答交给等待方。错误应答在这里记录一次（Host 给出的 code 与
    /// message），等待方再按各自的失败路径处理。
    pub(super) fn host_reply(
        &mut self,
        id: String,
        result: Result<serde_json::Value, crate::contract::HostError>,
    ) {
        if let Err(error) = &result {
            tracing::warn!(
                target: "zcode::runtime",
                event = "host.request.failed",
                request_id = %id,
                code = error.code,
                message = %error.message,
                "Host request failed"
            );
        }
        if let Some(wait) = self.waiters.take_host(&id) {
            let _ = wait.reply.send(result);
        }
    }

    /// An adapter's Host interaction: a notification, or a request keyed by
    /// its `requestId` whose reply is released only at shutdown otherwise.
    pub(super) fn host_call(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        reply: Option<
            tokio::sync::oneshot::Sender<Result<serde_json::Value, crate::contract::HostError>>,
        >,
    ) {
        let Some(reply) = reply else {
            self.outbox
                .push(crate::contract::ServerMsg::HostNotification { method, params });
            return;
        };
        let id = params["requestId"].as_str().unwrap_or_default().to_owned();
        let wait = super::waiters::HostWait {
            owner: method.into(),
            workspace: params["workspace"].clone(),
            reply,
        };
        self.waiters.add_host(id.clone(), wait);
        self.outbox
            .push(crate::contract::ServerMsg::HostRequest { id, method, params });
    }

    /// The session's plugin reference catalog, frozen on first use in this
    /// process (Node freezes it when the App is created).
    pub(super) async fn session_plugin_catalog(&mut self, id: &str) -> Result<Arc<[Value]>> {
        let session = self.sessions.get(id).context("Session unavailable")?;
        if let Some(catalog) = &session.runtime.plugin_catalog {
            return Ok(catalog.clone());
        }
        // 目录按会话所属 workspace 的插件配置构建。
        let catalog: Arc<[Value]> = self
            .session_tools(id)?
            .plugin_catalog(&CancellationToken::new())
            .await?
            .into();
        let session = self.sessions.get_mut(id).context("Session unavailable")?;
        Ok(session
            .runtime
            .plugin_catalog
            .get_or_insert(catalog)
            .clone())
    }

    /// Plugin reference events of a run; other events pass through.
    pub(super) async fn plugin_side(&mut self, event: RunEvent) -> Result<Option<RunEvent>> {
        let RunEvent {
            session_id: id,
            run_id,
            event,
        } = event;
        match event {
            Event::HostCall {
                method,
                params,
                reply,
            } => {
                self.host_call(method, params, reply);
                Ok(None)
            }
            Event::PluginCatalog { reply } => {
                match self.session_plugin_catalog(&id).await {
                    Ok(catalog) => {
                        let _ = reply.send(catalog);
                    }
                    // 目录不可用时引用提醒 fail open：不回复，运行照常继续。
                    Err(error) => tracing::debug!(
                        event = "plugin_reference.catalog_failed",
                        error = %format!("{error:#}"),
                        "Plugin reference catalog unavailable"
                    ),
                }
                Ok(None)
            }
            Event::ModelOnlyNotice {
                source,
                body,
                message,
                committed,
            } => {
                let live = self
                    .active
                    .get(&id)
                    .is_some_and(|a| a.run_id == run_id && !a.cancel.is_cancelled());
                if live && let Some(session) = self.sessions.get_mut(&id) {
                    session.append_message(message);
                    // 修复：提醒原先只进内存历史，冷恢复后丢失、请求前缀错位；Node 以 model-only
                    // notice 原文落库，hydration 按同一 source 重建提醒。
                    self.node_model_notice(&id, source, &body);
                    self.persist(&id, None).await?;
                    let _ = committed.send(());
                }
                Ok(None)
            }
            event => Ok(Some(RunEvent {
                session_id: id,
                run_id,
                event,
            })),
        }
    }
}
