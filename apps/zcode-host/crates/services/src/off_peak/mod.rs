//! 闲时任务（off-peak-task）：docs/specs/rust-host-automation.md 第 6 节。
//!
//! 组成：`off_peak_tasks` 仓库（tasks_index/off_peak_repo.rs）、服务端客户端与进程内 mock 网关、
//! 编排服务（取号、轮询、核销）、`off-peak-task` channel、`off-peak-run` 派发、CLI 反向请求
//! `offPeak/create|list`，以及本地 workspace 的 Off-Peak 工具面策略。
pub mod client;
pub mod dispatch;
pub mod gateway;
pub mod models;
pub mod reverse;
pub mod service;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use zcode_host_control::ControlRouter;
use zcode_host_kit::{
    Args, CallContext, CallFuture, ChannelRegistry, EventStream, Reply, RpcError, RpcValue,
    ServiceBuilder, ServiceChannel, log,
};

use crate::agent::extensions::{UseHook, UseReason};
use crate::automation::{Area, Keepalive};
use crate::tasks_index::off_peak_repo::OffPeakRepo;

use service::OffPeakService;

fn anyhow_error(error: anyhow::Error) -> RpcError {
    RpcError::new("Error", format!("{error:#}"))
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
            async move { Ok(Reply::Value(service.coding_plan_support())) }
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

/// 本地 workspace 在任何会话工作前推送 `workspace/updateOffPeakToolPolicy {enabled:true}`（每代进程一次）。
/// 旧 CLI 回 -32601 是正常降级；其他失败只记 warn，CLI 维持缺省关闭。
fn policy_hook() -> UseHook {
    let pushed: Arc<Mutex<HashSet<(String, u64)>>> = Arc::default();
    Arc::new(move |process, _reason: UseReason| {
        let pushed = pushed.clone();
        Box::pin(async move {
            if !dispatch::tool_supported(&process.target) {
                return Ok(());
            }
            let key = (process.key.clone(), process.generation);
            if !pushed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key)
            {
                return Ok(());
            }
            let params = json!({"workspace": process.target.workspace_ref(), "enabled": true});
            if let Err(error) = process
                .request_value("workspace/updateOffPeakToolPolicy", &params)
                .await
                && error.code_i64() != Some(-32601)
            {
                log::warn(format!(
                    "Off-Peak 工具策略同步失败，CLI 维持缺省关闭 workspaceKey={}: {}",
                    process.key, error.message
                ));
            }
            Ok(())
        })
    })
}

pub fn install(
    area: &Arc<Area>,
    registry: &mut ChannelRegistry,
    router: &mut ControlRouter,
) -> Option<Keepalive> {
    let vars = area.context.environment.vars.clone();
    let client = match client::ServerClient::new(vars.clone(), None) {
        Ok(client) => client,
        Err(error) => {
            log::error(format!("off-peak client unavailable: {error:#}"));
            return None;
        }
    };
    let bridge = area.bridge.clone();
    let driver = area.driver.clone();
    let service = OffPeakService::new(
        OffPeakRepo::new(area.tasks_index.clone()),
        client,
        area.providers.clone(),
        vars,
        Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as i64)
                .unwrap_or_default()
        }),
        Arc::new(move || bridge.post(json!({"type": "off-peak-scheduler-wake-request"}))),
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
    let run = dispatcher.clone();
    router.on(dispatch::OFF_PEAK_RUN, move |message| run.handle(message));
    area.agent.extensions().add_use_hook(policy_hook());
    Some(Box::new((service, dispatcher)))
}
