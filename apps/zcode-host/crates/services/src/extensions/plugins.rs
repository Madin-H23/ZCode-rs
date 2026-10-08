//! `plugins` channel：旧的 marketplace pluginStore 通道，Node 已退役（pluginsService.ts）。
//! 概览固定为空且 `capability.supported=false`，其余成员一律抛同一错误；插件管理走
//! `plugin-management`（agent 子系统）。
use serde_json::json;
use zcode_host_kit::{Reply, ServiceBuilder};

use super::fsx;

pub const CHANNEL: &str = "plugins";
const RETIRED: &str = "Legacy plugin management has been retired in ZCode Agent mode";

pub fn service() -> ServiceBuilder {
    let mut builder = ServiceBuilder::new(CHANNEL).method("getOverview", |_, _| async {
        Ok(Reply::Value(json!({
            "marketplaces": [],
            "availablePlugins": [],
            "installedPlugins": [],
            "capability": { "supported": false },
        })))
    });
    for method in [
        "addMarketplace",
        "removeMarketplace",
        "updateMarketplace",
        "installPlugin",
        "uninstallPlugin",
        "setPluginEnabled",
    ] {
        builder = builder.method(method, |_, _| async { Err(fsx::error(RETIRED)) });
    }
    builder
}
