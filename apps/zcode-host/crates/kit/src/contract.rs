//! 契约清单（`apps/zcode-host/contract/channels.json`，由 `scripts/rust-host/contract-inventory.mjs`
//! 从 Renderer 的 RemoteServiceAccess 生成）。
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemberKind {
    Method,
    Event,
    DynamicEvent,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Member {
    pub name: String,
    pub kind: MemberKind,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelContract {
    pub channel: String,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Contract {
    pub channels: Vec<ChannelContract>,
}

const EMBEDDED: &str = include_str!("../../../contract/channels.json");

/// 清单随二进制编译进来；格式错误属于构建产物损坏，测试会先拦住。
pub fn embedded() -> Contract {
    serde_json::from_str(EMBEDDED).expect("apps/zcode-host/contract/channels.json is invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_contract_lists_forty_channels() {
        let contract = embedded();
        assert_eq!(contract.channels.len(), 40);
        let setting = contract
            .channels
            .iter()
            .find(|channel| channel.channel == "setting")
            .unwrap();
        assert!(
            setting
                .members
                .iter()
                .any(|member| member.name == "get" && member.kind == MemberKind::Method)
        );
    }
}
