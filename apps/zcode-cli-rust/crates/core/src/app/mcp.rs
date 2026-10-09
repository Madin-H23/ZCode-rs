use super::{Engine, auxiliary::Auxiliary, engine::Call};
use crate::contract::{Event, EventSink, ModelFailure, ProcessCleanupFailure};
use anyhow::{Result, ensure};
use tokio_util::sync::CancellationToken;

impl Engine {
    pub(super) async fn start_mcp_query(&mut self, request: &Call) -> Result<()> {
        // 状态探测按请求的 workspace：路径作工作目录、配置从该 workspace 读取。
        let workspace = self.request_workspace(&request.params)?;
        ensure!(self.auxiliary.len() < 16, "Too many auxiliary requests");
        self.open_scope(&workspace).await?;
        let id = format!("mcp-query:{}", self.clock.id());
        let cancel = CancellationToken::new();
        self.auxiliary.insert(
            id.clone(),
            Auxiliary {
                token: request.token,
                cancel: cancel.clone(),
                operation: None,
                plugin_operation: None,
                title: None,
                workspace: workspace.clone(),
            },
        );
        let sink = EventSink {
            session_id: id.clone(),
            run_id: id,
            tx: self.events.clone(),
            origin: crate::contract::RequestOrigin::detached(self.clock.id()),
            request_auth: None,
        };
        let tools = self.scope(&workspace.key)?.tools.clone();
        let params = request.params.clone();
        self.tasks.spawner().spawn("mcp-query", async move {
            // panic 时按配置失败回复，mcp/list 的调用方不会永远挂起。
            let result = zcode_async::catch_panic(tools.mcp_list(&params, &cancel))
                .await
                .unwrap_or_else(|panicked| Err(super::panicked("mcp", panicked)));
            if result
                .as_ref()
                .is_err_and(anyhow::Error::is::<ProcessCleanupFailure>)
            {
                let _ = sink
                    .send(Event::ToolCleanupFailed(
                        "MCP process cleanup failed".into(),
                    ))
                    .await;
            } else {
                let result =
                    result.map_err(|_| ModelFailure::new("mcp_configuration_failed", false));
                let _ = sink.send(Event::AuxiliaryDone { result }).await;
            }
        });
        Ok(())
    }
}
