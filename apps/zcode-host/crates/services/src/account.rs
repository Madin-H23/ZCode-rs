//! 账号相关 channel 的未登录实现（docs/specs/rust-host.md R1；账号体系在 R4 移植）：
//! `oauth`、`onboarding-record`、`usage-stats`、`coding-plan-subscription` 中启动时会被调用的成员。
//! 返回值取自 TS 版在未登录、无本地任务时的真实结果（oauthService.ts:166-200 等）；
//! 其余成员仍由契约 stub 立即回 NotImplemented。
use std::collections::BTreeMap;

use serde_json::{Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use crate::providers::ProviderRuntime;

fn env_flag(vars: &BTreeMap<String, String>, key: &str, fallback: bool) -> bool {
    match vars
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        None => fallback,
        Some(value) => value != "0" && !value.eq_ignore_ascii_case("false"),
    }
}

fn constant(
    value: Value,
) -> impl Fn(
    zcode_host_kit::CallContext,
    zcode_host_kit::Args,
) -> std::future::Ready<Result<Reply, RpcError>>
+ Send
+ Sync
+ 'static {
    move |_, _| std::future::ready(Ok(Reply::Value(value.clone())))
}

fn undefined(
    _: zcode_host_kit::CallContext,
    _: zcode_host_kit::Args,
) -> std::future::Ready<Result<Reply, RpcError>> {
    std::future::ready(Ok(Reply::Undefined))
}

fn login_unavailable(
    _: zcode_host_kit::CallContext,
    _: zcode_host_kit::Args,
) -> std::future::Ready<Result<Reply, RpcError>> {
    std::future::ready(Err(RpcError::new(
        "Error",
        "Account login is not available in the Rust Host yet",
    )
    .with_code("fault.host.notImplemented")))
}

pub fn oauth_service(vars: &BTreeMap<String, String>) -> ServiceBuilder {
    let mut providers = Vec::new();
    if env_flag(vars, "BIGMODEL_OAUTH_ENABLED", true) {
        providers.push(
            json!({"id": "bigmodel", "displayName": "BigModel", "enabled": true, "order": 0}),
        );
    }
    if env_flag(vars, "ZAI_OAUTH_ENABLED", true) {
        providers.push(json!({"id": "zai", "displayName": "Z.ai", "enabled": true, "order": 1}));
    }
    ServiceBuilder::new("oauth")
        .method("getProviders", constant(Value::Array(providers)))
        .method("getActiveProvider", constant(Value::Null))
        .method("restoreCachedSession", constant(Value::Null))
        .method(
            "restoreCachedSessionState",
            constant(json!({"status": "signed-out"})),
        )
        .method("restoreSession", constant(Value::Null))
        .method("pollPendingOAuth", constant(Value::Null))
        .method("cancelPending", undefined)
        .method("logout", undefined)
        .method("logoutAll", undefined)
        .method("startOAuth", login_unavailable)
        .method("startOAuthWithPolling", login_unavailable)
        .method("handleCallback", login_unavailable)
        .method("refreshToken", login_unavailable)
}

/// 未登录时不弹职业引导（TS 版在没有本地任务时会弹，R4 随 onboarding 文件一起移植）。
pub fn onboarding_record_service() -> ServiceBuilder {
    ServiceBuilder::new("onboarding-record")
        .method("claimAnonymousRecord", undefined)
        .method("shouldOnboard", constant(Value::Bool(false)))
        .method("getLatestEntry", constant(Value::Null))
        .method("syncSettingsFromRecord", constant(Value::Null))
        .method("getRecords", constant(Value::Null))
        .method("dismissOnboarding", undefined)
        .method("appendRecord", undefined)
        .method("updateRecordPreferences", undefined)
        .method("clearRecords", undefined)
}

pub fn usage_stats_service() -> ServiceBuilder {
    ServiceBuilder::new("usage-stats").method("getEntitlementSnapshot", |_, _| async {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        Ok(Reply::Value(json!({
            "generatedAt": now,
            "authenticated": true,
            "unavailableReason": "not_configured",
            "context": {"scope": "personal"},
            "provider": null,
            "remaining": null,
            "subscription": null,
            "quota": null,
        })))
    })
}

pub fn coding_plan_service(providers: ProviderRuntime) -> ServiceBuilder {
    ServiceBuilder::new("coding-plan-subscription")
        .method(
            "getDynamicWorkflowClientConfig",
            constant(json!({"mode": "disabled", "enabled": false, "source": "default"})),
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
