//! 闲时任务（off-peak-task）：docs/specs/rust-host-automation.md 第 6 节。
//!
//! 组成：`off_peak_tasks` 仓库（tasks_index/off_peak_repo.rs）、服务端客户端与进程内 mock 网关、
//! 编排服务（取号、轮询、核销）、`off-peak-task` channel、`off-peak-run` 派发、CLI 反向请求
//! `offPeak/create|list`，以及本地 workspace 的 Off-Peak 工具面策略。
pub mod client;
pub mod credentials;
pub mod dispatch;
pub mod gateway;
pub mod models;
pub mod reverse;
pub mod service;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use serde_json::{Value, json};
use zcode_host_kit::{
    Args, CallContext, CallFuture, ChannelRegistry, EventStream, Reply, RpcError, RpcValue,
    ServiceBuilder, ServiceChannel, log,
};

use crate::automation::{Area, Keepalive};
use crate::tasks_index::off_peak_repo::OffPeakRepo;

use service::OffPeakService;

fn anyhow_error(error: anyhow::Error) -> RpcError {
    RpcError::new("Error", error.to_string())
}

fn id_arg(args: &Args) -> Result<String, RpcError> {
    args.get::<String>(0, "offPeakTaskId")
}

fn optional_value(value: Option<Value>) -> Reply {
    Reply::Value(value.unwrap_or(Value::Null))
}

/// `off-peak-task` channel（IOffPeakTaskService 的 11 个成员）。
pub fn channel(service: Arc<OffPeakService>) -> ServiceBuilder {
    let s = move || service.clone();
    let (a, b, c, d, e, f, g, h, i, j, k) = (s(), s(), s(), s(), s(), s(), s(), s(), s(), s(), s());
    ServiceBuilder::new("off-peak-task")
        .method("getCodingPlanSupport", move |_, _| {
            let service = a.clone();
            async move {
                service
                    .coding_plan_support()
                    .await
                    .map(Reply::Value)
                    .map_err(|message| RpcError::new("Error", message))
            }
        })
        .method("getTakeNumberAvailability", move |_, _| {
            let service = b.clone();
            async move {
                service
                    .take_number_availability()
                    .await
                    .map(Reply::Value)
                    .map_err(|message| RpcError::new("Error", message))
            }
        })
        .method("createTask", move |_, args| {
            let service = c.clone();
            async move {
                let params = args.raw(0).cloned().unwrap_or(Value::Null);
                service
                    .create_task(&params)
                    .await
                    .map(Reply::Value)
                    .map_err(|message| RpcError::new("Error", message))
            }
        })
        .method("cancelTask", move |_, args| {
            let service = d.clone();
            async move {
                let id = id_arg(&args)?;
                service
                    .cancel_task(&id)
                    .await
                    .map(optional_value)
                    .map_err(anyhow_error)
            }
        })
        .method("pauseTask", move |_, args| {
            let service = e.clone();
            async move {
                let id = id_arg(&args)?;
                service
                    .pause_task(&id)
                    .await
                    .map(optional_value)
                    .map_err(anyhow_error)
            }
        })
        .method("continueTask", move |_, args| {
            let service = f.clone();
            async move {
                let id = id_arg(&args)?;
                service
                    .continue_task(&id)
                    .await
                    .map(optional_value)
                    .map_err(anyhow_error)
            }
        })
        .method("deleteTask", move |_, args| {
            let service = g.clone();
            async move {
                let id = id_arg(&args)?;
                service.delete_task(&id).await.map_err(anyhow_error)?;
                Ok(Reply::Undefined)
            }
        })
        .method("deleteHistory", move |_, args| {
            let service = h.clone();
            async move {
                let id = id_arg(&args)?;
                service
                    .delete_history(&id)
                    .await
                    .map(optional_value)
                    .map_err(anyhow_error)
            }
        })
        .method("updateTask", move |_, args| {
            let service = i.clone();
            async move {
                let id = id_arg(&args)?;
                let params = args.raw(1).cloned().unwrap_or_else(|| json!({}));
                service
                    .update_task(&id, &params)
                    .await
                    .map(optional_value)
                    .map_err(anyhow_error)
            }
        })
        .method("list", move |_, _| {
            let service = j.clone();
            async move {
                service
                    .list()
                    .await
                    .map(|tasks| Reply::Value(Value::Array(tasks)))
                    .map_err(anyhow_error)
            }
        })
        .method("get", move |_, args| {
            let service = k.clone();
            async move {
                let id = id_arg(&args)?;
                service
                    .get(&id)
                    .await
                    .map(optional_value)
                    .map_err(anyhow_error)
            }
        })
}

/// `coding-plan-subscription.getOffPeakClientConfig` 的 mock 分支（Node 由 coding-plan 服务在
/// `ZCODE_OFFPEAK_MOCK=1` 时直接替代远端曝光配置）；其余成员与非 mock 时原样交给原 channel。
pub struct GrayOverlay {
    area: Arc<Area>,
    inner: Arc<dyn ServiceChannel>,
}

impl GrayOverlay {
    pub async fn client_config(
        area: &Arc<Area>,
        inner: &Arc<dyn ServiceChannel>,
    ) -> Result<Value, RpcError> {
        let vars = &area.context.environment.vars;
        if models::mock_enabled(vars) {
            let (revision, resolved) = area.providers.snapshot().await;
            let view = models::model_selection_view(&resolved, revision);
            return Ok(models::mock_client_config(vars, view));
        }
        let context = CallContext {
            connection: 0,
            attachment_id: "host-off-peak".into(),
            client_mode: zcode_host_control::ClientMode::DesktopContinuous,
            window_id: None,
        };
        match inner
            .call(&context, "getOffPeakClientConfig", vec![])
            .await?
        {
            Reply::Value(value) => Ok(value),
            Reply::RawJson(raw) => serde_json::from_str(&raw)
                .map_err(|error| RpcError::new("Error", error.to_string())),
            _ => Ok(Value::Null),
        }
    }
}

impl ServiceChannel for GrayOverlay {
    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        if method == "getOffPeakClientConfig"
            && models::mock_enabled(&self.area.context.environment.vars)
        {
            let (area, inner) = (self.area.clone(), self.inner.clone());
            return Box::pin(
                async move { Self::client_config(&area, &inner).await.map(Reply::Value) },
            );
        }
        self.inner.call(context, method, args)
    }

    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        self.inner.listen(context, event, arg)
    }

    fn connection_closed(&self, context: &CallContext) {
        self.inner.connection_closed(context);
    }
}

/// 安装结果：随 Host 存活的对象，以及交给 Scheduler 的派发器。
pub struct Installed {
    pub keepalive: Keepalive,
    pub dispatcher: Arc<dispatch::OffPeakDispatcher>,
}

pub fn install(area: &Arc<Area>, registry: &mut ChannelRegistry) -> Option<Installed> {
    let vars = area.context.environment.vars.clone();
    // 真实凭据链（docs/specs/rust-host-integration.md §2.1）：账号栈注入时用它，否则只有 mock。
    let credentials = credentials::CredentialSource::new(
        vars.clone(),
        area.account
            .as_ref()
            .map(|account| credentials::AccountDeps {
                credentials: area.credentials.clone(),
                auth: account.auth.clone(),
                providers: area.providers.clone(),
            }),
    );
    let source_headers = area
        .account
        .as_ref()
        .map(|account| account.source_headers.clone());
    let client = match client::ServerClient::new(vars, credentials, source_headers, None) {
        Ok(client) => client,
        Err(error) => {
            log::error(format!("off-peak client unavailable: {error:#}"));
            return None;
        }
    };
    let wake = area.scheduler_wake.clone();
    let driver = area.driver.clone();
    let service = OffPeakService::new(
        OffPeakRepo::new(area.tasks_index.clone()),
        client,
        area.providers.clone(),
        Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as i64)
                .unwrap_or_default()
        }),
        // U4：翻为可派发时直接唤醒 Host 内的 Scheduler，不再经 Main（off-peak-scheduler-wake-request）。
        Arc::new(move || wake.wake()),
        Some(Arc::new(move |conversation, path, identity| {
            let driver = driver.clone();
            Box::pin(async move {
                let target = crate::agent::WorkspaceTarget {
                    workspace_path: path,
                    workspace_identity: identity,
                    remote_session_id: None,
                };
                driver
                    .stop(&target, &conversation)
                    .await
                    .map_err(|error| error.message)
            })
        })),
    );
    service.start_sync();
    registry.implement(channel(service.clone()));
    if let Some(inner) = registry.get("coding-plan-subscription") {
        registry.register(
            "coding-plan-subscription",
            Arc::new(GrayOverlay {
                area: area.clone(),
                inner: inner.clone(),
            }),
        );
        reverse::register(area, service.clone(), inner);
    }
    let dispatcher = dispatch::OffPeakDispatcher::start(area.clone(), service.clone());
    // 合并 automation 与 agent 两个区域时发现两条推送 updateOffPeakToolPolicy 的写路径（这里的使用前钩子
    // 与 agent runtime 的启动回放）。统一由 agent runtime 在每代 CLI 启动时推送（同 Node 的 replay），
    // 这里只打开门禁；门禁同时让 createSession 带上 offPeakToolEnabled。
    area.agent.extensions().enable_off_peak_tools();
    Some(Installed {
        keepalive: Box::new((service, dispatcher.clone())),
        dispatcher,
    })
}
