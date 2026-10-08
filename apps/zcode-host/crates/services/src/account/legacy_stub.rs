//! `usage-stats`、`coding-plan-subscription` 中启动时会被调用的成员的占位实现（R4 移植前）。
use serde_json::json;
use zcode_host_kit::{Reply, ServiceBuilder};

use crate::providers::ProviderRuntime;

pub fn coding_plan_service(providers: ProviderRuntime) -> ServiceBuilder {
    ServiceBuilder::new("coding-plan-subscription")
        .method(
            "getDynamicWorkflowClientConfig",
            |_, _| async {
                Ok(Reply::Value(
                    json!({"mode": "disabled", "enabled": false, "source": "default"}),
                ))
            },
        )
        .method("getOffPeakClientConfig", move |_, _| {
            let providers = providers.clone();
            async move {
                let (revision, resolved) = providers.snapshot().await;
                Ok(Reply::Value(json!({
                    "enabled": false,
                    "modelSelectionView": resolved.model_selection_view(revision, None),
                })))
            }
        })
}
