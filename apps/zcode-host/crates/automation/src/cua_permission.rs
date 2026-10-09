//! cua-permission（packages/services/src/node.ts 的 `cuaPermissionService`）。
//!
//! 本构建里 Computer Use（`@zcode/zcode-cua`）是占位实现，Helper 无法安装或启动，所以 Rust Host
//! fail closed：返回 Node Host 在“非 macOS”与“Computer Use 未启用”时的同一结果，从不拉起 Helper。
//! `cua-pip-session` 不在 Renderer 契约里，保持不实现（同样 fail closed）。
use std::sync::Arc;

use serde_json::json;
use zcode_host_control::ControlRouter;
use zcode_host_kit::{ChannelRegistry, Reply, ServiceBuilder};

use crate::automation::{Area, Keepalive};

const NOT_MACOS: &str = "CUA permissions are only available on macOS.";
const NOT_ENABLED: &str = "ZCode Computer Use is not enabled (plugin off or not product mode).";

fn reason() -> &'static str {
    if cfg!(target_os = "macos") {
        NOT_ENABLED
    } else {
        NOT_MACOS
    }
}

pub fn service() -> ServiceBuilder {
    ServiceBuilder::new("cua-permission")
        .method("getStatus", |_, _| async {
            Ok(Reply::Value(
                json!({"available": false, "reason": reason()}),
            ))
        })
        .method("restartHelper", |_, _| async {
            Ok(Reply::Value(json!({"ok": false, "reason": reason()})))
        })
}

pub fn install(
    _area: &Arc<Area>,
    registry: &mut ChannelRegistry,
    _router: &mut ControlRouter,
) -> Option<Keepalive> {
    registry.implement(service());
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcode_host_control::ClientMode;
    use zcode_host_kit::CallContext;

    #[tokio::test]
    async fn fails_closed_like_the_node_host() {
        let mut registry = ChannelRegistry::empty();
        registry.implement(service());
        let channel = registry.get("cua-permission").unwrap();
        let context = CallContext {
            connection: 1,
            attachment_id: "a".into(),
            client_mode: ClientMode::DesktopContinuous,
        };
        let status = channel.call(&context, "getStatus", vec![]).await.unwrap();
        assert_eq!(
            status,
            Reply::Value(json!({"available": false, "reason": reason()}))
        );
        let restart = channel
            .call(&context, "restartHelper", vec![])
            .await
            .unwrap();
        assert_eq!(
            restart,
            Reply::Value(json!({"ok": false, "reason": reason()}))
        );
    }
}
