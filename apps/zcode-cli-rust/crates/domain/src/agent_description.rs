//! Pure formatting of Node's Agent tool description; never changes profile policy.
use super::subagent::{Profile, builtins};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    default_catalog: String,
    header: String,
    without_tools: [String; 3],
    with_tools: [String; 4],
    explore_tools: String,
}

pub fn description(base: &str, profiles: &[Profile]) -> String {
    let catalog: Catalog =
        serde_json::from_str(include_str!("agent_catalog.json")).expect("validated Agent catalog");
    let defaults = builtins();
    let mut ordered: Vec<&Profile> = defaults.iter().collect();
    // Node normalizeAgentProfiles 的顺序仅用于描述：同名覆盖原位置，其他保持输入顺序。
    // 不修改 discovery 结果或传入 profile，避免文本对齐影响子代理执行策略。
    for profile in profiles {
        if let Some(slot) = ordered.iter_mut().find(|p| p.name == profile.name) {
            *slot = profile;
        } else {
            ordered.push(profile);
        }
    }
    let mut lines = vec![catalog.header.clone()];
    for profile in ordered {
        let tools = if profile.name == "Explore" && profile.source == "built-in" {
            catalog.explore_tools.clone()
        } else {
            profile
                .tools
                .iter()
                .flatten()
                .filter(|name| {
                    let name = alias(name);
                    !matches!(name, "EnterPlanMode" | "ExitPlanMode")
                        && !profile.disallowed_tools.iter().any(|rule| {
                            let rule = rule_name(rule);
                            !rule.is_empty() && rule == name
                        })
                })
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        };
        // 用片段拼接，不把 profile 原文当作占位符模板再次替换。
        let line = if tools.is_empty() {
            let [start, middle, end] = &catalog.without_tools;
            format!(
                "{start}{}{middle}{}{end}",
                profile.name, profile.description
            )
        } else {
            let [start, middle, before_tools, end] = &catalog.with_tools;
            format!(
                "{start}{}{middle}{}{before_tools}{tools}{end}",
                profile.name, profile.description
            )
        };
        lines.push(line);
    }
    // 原来末尾追加第二份目录；Node 只在原目录位置拼接一次完整目录。
    base.replacen(&catalog.default_catalog, &lines.join("\n"), 1)
}

// Node tool-visibility 的名称规则只用于 Tools 后缀；不能复用执行层的通配匹配。
fn rule_name(rule: &str) -> &str {
    let rule = super::js_string::trim(rule);
    let name = match rule.find('(') {
        Some(at) if at > 0 => &rule[..at],
        _ => rule,
    };
    alias(name)
}

fn alias(name: &str) -> &str {
    if name == "web_search" {
        "WebSearch"
    } else {
        name
    }
}
