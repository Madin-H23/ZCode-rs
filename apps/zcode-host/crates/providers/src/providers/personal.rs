//! `provider-settings` 的写成员（packages/provider/src/config-service.ts、facades.ts 的
//! ProviderSettingsFacade 写路径、services/src/model-provider/providerSettingsConnectivity.ts）。
//!
//! 每个写操作：取当前快照（含 membership：继承模型名单与个人 revision）→ 持锁读文件 → 核对
//! revision → 变换 → 规范化校验 → 私有原子写 → 重新加载并返回新视图。整个编辑在 personal 文件的
//! owner 任务里执行，本进程内的编辑按提交顺序逐个进行（docs/specs/rust-u2-task-scope.md §6.3）。
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use sha2::Digest;
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use super::ProviderRuntime;
use super::personal_schema::{
    ModelRule, Personal, ProviderContext, canonical_model_config, canonical_provider_config,
    decode, encode, overlay_model_config, validate_manual_model_config, validate_model_config,
    validate_provider_config,
};
use super::resolve::{Resolved, effective_model_config, overlay};
use crate::store::{LockOptions, atomic_write_private_text, with_file_lock};

fn error(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}

fn normalize_id(label: &str, value: &str) -> Result<String, RpcError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(error(format!("{label} 不能为空")));
    }
    Ok(trimmed.to_owned())
}

fn unique_in_order(values: &[String]) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for value in values {
        if !result.contains(value) {
            result.push(value.clone());
        }
    }
    result
}

/// `resolveOwnedOrder`：未请求的内置 → 请求顺序（只含已知 id）→ 未请求的个人。
fn owned_order(builtin: &[String], personal: &[String], requested: &[String]) -> Vec<String> {
    let requested: Vec<&String> = requested
        .iter()
        .filter(|id| builtin.contains(id) || personal.contains(id))
        .collect();
    let mut result: Vec<String> = Vec::new();
    for id in builtin
        .iter()
        .filter(|id| !requested.contains(id))
        .chain(requested.iter().copied())
        .chain(personal.iter().filter(|id| !requested.contains(id)))
    {
        if !result.contains(id) {
            result.push(id.clone());
        }
    }
    result
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// 内置层（规则与模板）只读视图。
struct Builtin<'a>(&'a Value);

impl Builtin<'_> {
    fn provider(&self, id: &str) -> Option<&Value> {
        self.0["providerConfigRules"]["providerRules"]
            .as_array()?
            .iter()
            .find(|rule| rule["providerId"] == id)
            .map(|rule| &rule["config"])
    }

    fn provider_ids(&self) -> Vec<String> {
        self.0["providerConfigRules"]["providerRules"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|rule| rule["providerId"].as_str().map(str::to_owned))
            .collect()
    }

    fn template(&self, id: &str) -> Option<&Value> {
        self.0["providerConfigRules"]["templateRules"]
            .as_array()?
            .iter()
            .find(|template| template["templateId"] == id)
    }
}

/// 写入时的 membership（`ProviderModelMembership`）。
struct Membership {
    provider_id: String,
    inherited: Vec<String>,
    personal_revision: String,
}

impl Personal {
    fn rule_index(&self, id: &str) -> Option<usize> {
        self.providers
            .iter()
            .position(|rule| rule["providerId"] == id)
    }

    fn config(&self, id: &str) -> Option<&Value> {
        self.rule_index(id)
            .map(|index| &self.providers[index]["config"])
    }

    fn rule(&self, id: &str) -> Option<&Value> {
        self.rule_index(id).map(|index| &self.providers[index])
    }

    /// `ProviderConfigMap.set`：保留外层身份与名称，只换 config；不存在时追加。
    fn set_config(&mut self, id: &str, config: Value) {
        match self.rule_index(id) {
            Some(index) => self.providers[index]["config"] = config,
            None => self
                .providers
                .push(json!({"providerId": id, "config": config})),
        }
    }

    fn get_exact(&self, provider: &str, model: &str) -> Option<Value> {
        let mut result: Option<Value> = None;
        for rule in self
            .models
            .iter()
            .filter(|rule| rule.provider_id == provider && rule.model_id == model)
        {
            result = Some(match result {
                Some(current) => overlay_model_config(&current, &rule.config),
                None => rule.config.clone(),
            });
        }
        result
    }

    fn exact_rule(&self, provider: &str, model: &str) -> Option<&ModelRule> {
        self.models
            .iter()
            .rev()
            .find(|rule| rule.provider_id == provider && rule.model_id == model)
    }

    /// `ModelConfigRules.setExact`：首个同名规则原位替换，其余删除；不存在时追加。
    fn set_exact(
        &mut self,
        provider: &str,
        model: &str,
        config: &Value,
        use_recommended: Option<bool>,
    ) -> Result<(), RpcError> {
        let manual = match use_recommended {
            None => self
                .exact_rule(provider, model)
                .is_some_and(|rule| rule.manual),
            Some(recommended) => !recommended,
        };
        let config = canonical_model_config(config);
        let check = if manual {
            validate_manual_model_config(&config, "config")
        } else {
            validate_model_config(&config, "config")
        };
        check.map_err(|message| RpcError::new("ZodError", message))?;
        let replacement = ModelRule {
            manual,
            provider_id: provider.to_owned(),
            model_id: model.to_owned(),
            config,
        };
        let mut replaced = false;
        let mut next = Vec::with_capacity(self.models.len() + 1);
        for rule in self.models.drain(..) {
            if rule.provider_id == provider && rule.model_id == model {
                if !replaced {
                    next.push(replacement.clone());
                }
                replaced = true;
            } else {
                next.push(rule);
            }
        }
        if !replaced {
            next.push(replacement);
        }
        self.models = next;
        Ok(())
    }

    fn delete_exact(&mut self, provider: &str, model: &str) {
        self.models
            .retain(|rule| !(rule.provider_id == provider && rule.model_id == model));
    }

    fn rename_exact(&mut self, provider: &str, current: &str, next: &str) {
        for rule in &mut self.models {
            if rule.provider_id == provider && rule.model_id == current {
                rule.model_id = next.to_owned();
            }
        }
    }
}

fn is_structurally_empty(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.values().all(is_structurally_empty),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// `normalizeProviderOrder`：只排 standard-personal 的个人 provider。
fn normalize_provider_order(personal: &Personal, requested: &[String]) -> Vec<String> {
    let ids: Vec<String> = personal
        .providers
        .iter()
        .filter(|rule| rule["config"]["group"] == "standard-personal")
        .filter_map(|rule| rule["providerId"].as_str().map(str::to_owned))
        .collect();
    owned_order(&[], &ids, requested)
}

fn template_baseline<'a>(builtin: &'a Builtin, personal: &Personal, id: &str) -> Option<&'a Value> {
    let template_id = personal.rule(id)?["templateId"].as_str()?;
    builtin
        .template(template_id)
        .map(|template| &template["config"])
}

fn writable_overlay(builtin: &Builtin, personal: &Personal, id: &str) -> Result<Value, RpcError> {
    if let Some(config) = personal.config(id) {
        return Ok(config.clone());
    }
    if builtin.provider(id).is_some() {
        return Ok(json!({}));
    }
    Err(error(format!("Provider 不存在: {id}")))
}

/// `normalizePersonalProviderMembership`。
fn normalized_membership(
    personal_config: Option<&Value>,
    baseline: Option<&Value>,
    inherited: Option<&[String]>,
) -> Option<Value> {
    let personal_config = personal_config?;
    let builtin_ids = unique_in_order(&inherited.map(<[String]>::to_vec).unwrap_or_else(|| {
        baseline
            .map(|config| strings(&config["builtinModelIds"]))
            .unwrap_or_default()
    }));
    let personal_ids: Vec<String> = unique_in_order(&strings(&personal_config["personalModelIds"]))
        .into_iter()
        .filter(|id| !builtin_ids.contains(id))
        .collect();
    let mut normalized = personal_config.clone();
    normalized["personalModelIds"] = json!(personal_ids);
    if let Some(order) = personal_config
        .get("modelOrder")
        .filter(|order| !order.is_null())
    {
        normalized["modelOrder"] = json!(owned_order(&builtin_ids, &personal_ids, &strings(order)));
    }
    Some(normalized)
}

/// `withModelMembershipFrom`：成员字段只来自来源，其余来自本身。
fn with_membership_from(config: &Value, source: Option<&Value>) -> Value {
    let mut next = config.clone();
    if let Some(object) = next.as_object_mut() {
        for key in ["builtinModelIds", "personalModelIds", "modelOrder"] {
            object.remove(key);
            if let Some(value) = source.and_then(|source| source.get(key)) {
                object.insert(key.to_owned(), value.clone());
            }
        }
    }
    canonical_provider_config(&next)
}

fn label_of(rule: &Value) -> Option<String> {
    rule["providerName"]
        .as_str()
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
}

/// 内置 + 个人合并后的 provider 规则（只比较名称）。
fn effective_rules(builtin: &Builtin, personal: &Personal) -> Vec<Value> {
    let mut rules: Vec<Value> = builtin.0["providerConfigRules"]["providerRules"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for rule in &personal.providers {
        match rules
            .iter_mut()
            .find(|existing| existing["providerId"] == rule["providerId"])
        {
            Some(existing) => {
                if rule.get("providerName").is_some() {
                    existing["providerName"] = rule["providerName"].clone();
                }
            }
            None => rules.push(rule.clone()),
        }
    }
    rules
}

fn next_provider_id(occupied: &[String], template_id: Option<&str>) -> String {
    let base = match template_id {
        Some(template) => {
            let lowered = template.trim().to_lowercase();
            let mut seed = String::new();
            let mut dash = false;
            for c in lowered.chars() {
                if c.is_ascii_lowercase() || c.is_ascii_digit() {
                    seed.push(c);
                    dash = false;
                } else if !dash {
                    seed.push('-');
                    dash = true;
                }
            }
            let seed = seed.trim_matches('-').to_owned();
            if seed.is_empty() {
                "new-provider".to_owned()
            } else {
                seed
            }
        }
        None => "new-provider".to_owned(),
    };
    if !occupied.contains(&base) {
        return base;
    }
    (2..)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !occupied.contains(candidate))
        .expect("unbounded suffix")
}

fn next_provider_label(seed: &str, rules: &[Value]) -> String {
    let base = if seed.trim().is_empty() {
        "new-provider".to_owned()
    } else {
        seed.trim().to_owned()
    };
    let labels: Vec<String> = rules.iter().filter_map(label_of).collect();
    if !labels.contains(&base.to_lowercase()) {
        return base;
    }
    (2..)
        .map(|suffix| format!("{base} {suffix}"))
        .find(|candidate| !labels.contains(&candidate.to_lowercase()))
        .expect("unbounded suffix")
}

#[derive(Clone)]
pub struct PersonalWriter {
    runtime: ProviderRuntime,
    file: PathBuf,
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl PersonalWriter {
    pub fn new(runtime: ProviderRuntime) -> Self {
        let file = runtime.personal_file().to_path_buf();
        Self { runtime, file }
    }

    /// 读改写 personal 文件；`transform` 拿到 (当前配置, 当前 revision, 内置层)。只在 owner 任务里
    /// （[`Self::serialized`] 的编辑中）调用。
    async fn update<R: Send>(
        &self,
        transform: impl FnOnce(&mut Personal, &str, &Builtin) -> Result<R, RpcError> + Send,
    ) -> Result<R, RpcError> {
        let (_, builtin) = self.runtime.builtin_snapshot().await;
        let file = self.file.clone();
        let mut transform = Some(transform);
        let outcome: anyhow::Result<Result<R, RpcError>> =
            with_file_lock(&file, &LockOptions::default(), async || {
                let bytes = match tokio::fs::read(&file).await {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                    Err(error) => return Ok(Err(RpcError::new("Error", error.to_string()))),
                };
                let revision = sha256_hex(&bytes);
                let mut personal = if bytes.is_empty() {
                    Personal {
                        provider_order: Some(Vec::new()),
                        ..Personal::default()
                    }
                } else {
                    let value: Value = match serde_json::from_slice(&bytes) {
                        Ok(value) => value,
                        Err(error) => {
                            return Ok(Err(RpcError::new("SyntaxError", error.to_string())));
                        }
                    };
                    match decode(&value) {
                        Ok(personal) => personal,
                        Err(message) => return Ok(Err(RpcError::new("ZodError", message))),
                    }
                };
                let transform = transform.take().expect("single call");
                let result = match transform(&mut personal, &revision, &Builtin(&builtin)) {
                    Ok(result) => result,
                    Err(error) => return Ok(Err(error)),
                };
                // 写前按读取同一规范形态再校验整份结果，不能落盘后才发现坏值。
                let canonical = match decode(&encode(&personal)) {
                    Ok(canonical) => canonical,
                    Err(message) => return Ok(Err(RpcError::new("ZodError", message))),
                };
                let text = serde_json::to_string_pretty(&encode(&canonical)).unwrap_or_default();
                if let Err(error) = atomic_write_private_text(&file, &text).await {
                    return Ok(Err(RpcError::new("Error", format!("{error:#}"))));
                }
                Ok(Ok(result))
            })
            .await;
        let result = outcome.map_err(|error| RpcError::new("Error", format!("{error:#}")))??;
        self.runtime.reload().await;
        // 只有本进程成功提交的写入才通知（Node `personal:updated`），远端 Environment 据此重新同步。
        self.runtime.notify_personal_updated();
        Ok(result)
    }

    async fn membership(&self, provider_id: &str) -> Result<Membership, RpcError> {
        let (_, resolved, personal_revision) = self.runtime.snapshot_with_personal().await;
        membership_of(&resolved, provider_id, personal_revision)
    }

    async fn view(&self) -> Value {
        let (revision, resolved) = self.runtime.snapshot().await;
        resolved.provider_settings_view(revision)
    }

    /// 在 personal 文件的 owner 任务里执行一次编辑（读成员关系 → 读改写 → 读视图）：本进程内对这个文件
    /// 的编辑按提交顺序逐个执行，同一 provider 的并发编辑不会因为读到旧 revision 而误报冲突
    /// （docs/specs/rust-u2-task-scope.md §6.3）。编辑方放弃等待不打断已经开始的编辑。
    async fn serialized<R, Edit, Edited>(&self, edit: Edit) -> Result<R, RpcError>
    where
        Edit: FnOnce(PersonalWriter) -> Edited + Send + 'static,
        Edited: std::future::Future<Output = Result<R, RpcError>> + Send,
        R: Send + 'static,
    {
        let writer = self.clone();
        self.runtime
            .personal_writes()
            .run(async move { edit(writer).await })
            .await
            .unwrap_or_else(|stopped| Err(RpcError::new("Error", stopped.to_string())))
    }

    pub async fn create_provider(&self, input: &Value) -> Result<Value, RpcError> {
        let template_id = input["templateId"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        let provider_name = input["providerName"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        let locale = input["locale"].as_str().unwrap_or("en-US").to_owned();
        let initial = match input.get("initialConfig").filter(|v| !v.is_null()) {
            Some(config) => {
                validate_provider_config(config, ProviderContext::Input, "initialConfig")
                    .map_err(|message| RpcError::new("ZodError", message))?;
                if config.get("group").is_some() {
                    return Err(error("initialConfig 不能包含 group"));
                }
                if config.get("builtinModelIds").is_some() {
                    return Err(error("initialConfig 不能包含 builtinModelIds"));
                }
                Some(config.clone())
            }
            None => None,
        };
        self.serialized(move |writer| async move {
            let created = writer
                .update(move |personal, _, builtin| {
                    let template = match &template_id {
                        Some(id) => Some(
                            builtin
                                .template(id)
                                .ok_or_else(|| error(format!("Provider Template 不存在: {id}")))?
                                .clone(),
                        ),
                        None => None,
                    };
                    let mut occupied = builtin.provider_ids();
                    occupied.extend(
                        personal
                            .providers
                            .iter()
                            .filter_map(|rule| rule["providerId"].as_str().map(str::to_owned)),
                    );
                    let provider_id = next_provider_id(&occupied, template_id.as_deref());
                    let seed =
                        provider_name
                            .clone()
                            .unwrap_or_else(|| match (&template, &template_id) {
                                (Some(template), Some(id)) => {
                                    let names = &template["templateNameMap"];
                                    names[locale.as_str()]
                                        .as_str()
                                        .map(str::trim)
                                        .filter(|n| !n.is_empty())
                                        .or_else(|| {
                                            names["en-US"]
                                                .as_str()
                                                .map(str::trim)
                                                .filter(|n| !n.is_empty())
                                        })
                                        .unwrap_or(id)
                                        .to_owned()
                                }
                                _ => "new-provider".to_owned(),
                            });
                    let label = next_provider_label(&seed, &effective_rules(builtin, personal));
                    let mut config = json!({"group": "standard-personal"});
                    if template_id.is_none() {
                        config["access"] = json!({"type": "api-key"});
                    }
                    config["personalModelIds"] = json!([]);
                    config["modelOrder"] = json!([]);
                    if let Some(initial) = &initial {
                        overlay(&mut config, initial);
                    }
                    let mut rule = json!({"providerId": provider_id});
                    if let Some(id) = &template_id {
                        rule["templateId"] = json!(id);
                    }
                    rule["providerName"] = json!(label);
                    rule["config"] = canonical_provider_config(&config);
                    personal.providers.push(rule);
                    let current = normalize_provider_order(
                        personal,
                        personal.provider_order.as_deref().unwrap_or(&[]),
                    );
                    // 先排除再把新 provider 加到末尾。
                    let mut requested: Vec<String> = current
                        .into_iter()
                        .filter(|id| *id != provider_id)
                        .collect();
                    requested.push(provider_id.clone());
                    personal.provider_order = Some(normalize_provider_order(personal, &requested));
                    Ok(provider_id)
                })
                .await?;
            Ok(json!({"providerId": created, "view": writer.view().await}))
        })
        .await
    }

    pub async fn save_provider_overlay(
        &self,
        provider_id: &str,
        config: &Value,
        metadata: Option<&Value>,
    ) -> Result<Value, RpcError> {
        if provider_id.trim().is_empty() {
            return Err(error("providerId 不能为空"));
        }
        validate_provider_config(config, ProviderContext::Input, "config")
            .map_err(|message| RpcError::new("ZodError", message))?;
        let config = config.clone();
        let metadata = metadata.cloned().unwrap_or(Value::Null);
        let id = provider_id.to_owned();
        self.serialized(move |writer| async move {
            let membership = writer.membership(&id).await?;
            writer.update(move |personal, revision, builtin| {
                check_membership(&membership, &id, revision)?;
                let builtin_config = builtin.provider(&id);
                let account =
                    builtin_config.is_some_and(|c| c["access"]["type"] == "zhipu-account");
                if account && metadata["enabled"] == false {
                    return Err(error(format!("Account Provider 不允许禁用: {id}")));
                }
                if account && config.get("access").is_some() {
                    return Err(error(format!(
                        "固定 Account Provider 的 Access 只能由 ZCode Built-in Config 声明: {id}"
                    )));
                }
                let current_personal = personal.config(&id).cloned();
                if current_personal.is_none() && builtin_config.is_none() {
                    return Err(error(format!("Personal Provider 尚未创建: {id}")));
                }
                let mut normalized = config.clone();
                if let Some(builtin_config) = builtin_config {
                    if normalized.get("group").is_some_and(|g| !g.is_null())
                        && normalized["group"] != builtin_config["group"]
                    {
                        return Err(error(format!(
                            "Personal Overlay 不能改写 Built-in Provider group: {id}"
                        )));
                    }
                    if let Some(object) = normalized.as_object_mut() {
                        object.remove("group");
                    }
                } else {
                    let group = match normalized.get("group").filter(|g| !g.is_null()) {
                        Some(group) => group.clone(),
                        None => current_personal
                            .as_ref()
                            .map(|c| c["group"].clone())
                            .unwrap_or(Value::Null),
                    };
                    if group != "standard-personal" {
                        return Err(error(format!(
                            "Personal-only Provider 必须使用 standard-personal group: {id}"
                        )));
                    }
                    normalized["group"] = group;
                }
                let baseline = builtin_config
                    .or_else(|| template_baseline(builtin, personal, &id))
                    .cloned();
                let next = with_membership_from(
                    &normalized,
                    normalized_membership(
                        current_personal.as_ref(),
                        baseline.as_ref(),
                        Some(&membership.inherited),
                    )
                    .as_ref(),
                );
                let previous_label = personal.rule(&id).and_then(label_of);
                let before_rules = effective_rules(builtin, personal);
                let index = personal.rule_index(&id);
                let mut rule = index
                    .map(|i| personal.providers[i].clone())
                    .unwrap_or_else(|| json!({"providerId": id}));
                if let Some(template) = metadata.get("templateId") {
                    rule["templateId"] = template.clone();
                }
                if let Some(enabled) = metadata.get("enabled") {
                    rule["enabled"] = enabled.clone();
                }
                if let Some(name) = metadata.get("providerName") {
                    rule["providerName"] =
                        match name.as_str().map(str::trim).filter(|n| !n.is_empty()) {
                            Some(name) => json!(name),
                            None => Value::Null,
                        };
                }
                rule["config"] = next;
                match index {
                    Some(i) => personal.providers[i] = rule,
                    None => personal.providers.push(rule),
                }
                // 只禁止本次改名新引入重名；历史重名不阻断其他保存。
                let next_label = personal.rule(&id).and_then(label_of).or_else(|| {
                    before_rules
                        .iter()
                        .find(|rule| rule["providerId"] == id.as_str())
                        .and_then(label_of)
                });
                if let Some(label) = next_label
                    && Some(&label) != previous_label.as_ref()
                {
                    let clash = effective_rules(builtin, personal).iter().any(|rule| {
                        rule["providerId"] != id.as_str()
                            && label_of(rule).as_deref() == Some(label.as_str())
                    });
                    if clash {
                        let display = personal
                            .rule(&id)
                            .and_then(|r| {
                                r["providerName"].as_str().map(str::trim).map(str::to_owned)
                            })
                            .unwrap_or(label);
                        return Err(error(format!("Provider 名称已存在: {display}")));
                    }
                }
                Ok(())
            })
            .await?;
            Ok(writer.view().await)
        })
        .await
    }

    pub async fn delete_provider(&self, provider_id: &str) -> Result<Value, RpcError> {
        if provider_id.trim().is_empty() {
            return Err(error("providerId 不能为空"));
        }
        let id = provider_id.to_owned();
        self.serialized(move |writer| async move {
            writer
                .update(move |personal, _, _| {
                    personal
                        .providers
                        .retain(|rule| rule["providerId"] != id.as_str());
                    personal.models.retain(|rule| rule.provider_id != id);
                    if let Some(order) = &mut personal.provider_order {
                        order.retain(|candidate| *candidate != id);
                    }
                    Ok(())
                })
                .await?;
            Ok(writer.view().await)
        })
        .await
    }

    pub async fn reorder_providers(&self, provider_ids: Vec<String>) -> Result<Value, RpcError> {
        self.serialized(move |writer| async move {
            writer
                .update(move |personal, _, _| {
                    personal.provider_order =
                        Some(normalize_provider_order(personal, &provider_ids));
                    Ok(())
                })
                .await?;
            Ok(writer.view().await)
        })
        .await
    }

    pub async fn reorder_models(
        &self,
        provider_id: &str,
        model_ids: Vec<String>,
    ) -> Result<Value, RpcError> {
        if provider_id.trim().is_empty() {
            return Err(error("providerId 不能为空"));
        }
        let id = provider_id.to_owned();
        self.serialized(move |writer| async move {
            let membership = writer.membership(&id).await?;
            writer
                .update(move |personal, revision, builtin| {
                    check_membership(&membership, &id, revision)?;
                    let provider = writable_overlay(builtin, personal, &id)?;
                    let order = owned_order(
                        &membership.inherited,
                        &strings(&provider["personalModelIds"]),
                        &model_ids,
                    );
                    let mut next = provider;
                    next["modelOrder"] = json!(order);
                    personal.set_config(&id, canonical_provider_config(&next));
                    Ok(())
                })
                .await?;
            Ok(writer.view().await)
        })
        .await
    }

    pub async fn add_model(
        &self,
        provider_id: &str,
        model_id: &str,
        config: &Value,
        use_recommended: Option<bool>,
    ) -> Result<Value, RpcError> {
        let id = normalize_id("providerId", provider_id)?;
        let model = normalize_id("modelId", model_id)?;
        validate_model_config(config, "config")
            .map_err(|message| RpcError::new("ZodError", message))?;
        let config = config.clone();
        self.serialized(move |writer| async move {
            let membership = writer.membership(&id).await?;
            writer
                .update(move |personal, revision, builtin| {
                    check_membership(&membership, &id, revision)?;
                    let provider = writable_overlay(builtin, personal, &id)?;
                    if membership.inherited.contains(&model) {
                        return Err(error(format!("Model 已存在: {id}/{model}")));
                    }
                    let mut personal_ids = strings(&provider["personalModelIds"]);
                    if personal_ids.contains(&model) {
                        return Err(error(format!("Model 已存在: {id}/{model}")));
                    }
                    personal_ids.push(model.clone());
                    let order = owned_order(
                        &membership.inherited,
                        &personal_ids,
                        &strings(&provider["modelOrder"]),
                    );
                    let mut next = provider;
                    next["personalModelIds"] = json!(personal_ids);
                    next["modelOrder"] = json!(order);
                    personal.set_config(&id, canonical_provider_config(&next));
                    let enabled = overlay_model_config(&config, &json!({"enabled": true}));
                    personal.set_exact(&id, &model, &enabled, use_recommended)?;
                    Ok(())
                })
                .await?;
            Ok(writer.view().await)
        })
        .await
    }

    pub async fn rename_model(
        &self,
        provider_id: &str,
        current: &str,
        next_id: &str,
    ) -> Result<Value, RpcError> {
        let id = normalize_id("providerId", provider_id)?;
        let current = normalize_id("modelId", current)?;
        let next_model = normalize_id("modelId", next_id)?;
        if current == next_model {
            return Ok(self.view().await);
        }
        self.serialized(move |writer| async move {
            let membership = writer.membership(&id).await?;
            writer
                .update(move |personal, revision, _| {
                    check_membership(&membership, &id, revision)?;
                    let inherited = &membership.inherited;
                    if inherited.contains(&current) {
                        return Err(error(format!("Built-in Model 不能重命名: {id}/{current}")));
                    }
                    let Some(provider) = personal.config(&id).cloned() else {
                        return Err(error(format!("Personal Model 不存在: {id}/{current}")));
                    };
                    let personal_ids = strings(&provider["personalModelIds"]);
                    if !personal_ids.contains(&current) {
                        return Err(error(format!("Personal Model 不存在: {id}/{current}")));
                    }
                    if personal_ids.contains(&next_model) || inherited.contains(&next_model) {
                        return Err(error(format!("Model 已存在: {id}/{next_model}")));
                    }
                    let rename = |ids: Vec<String>| -> Vec<String> {
                        ids.into_iter()
                            .map(|m| if m == current { next_model.clone() } else { m })
                            .collect()
                    };
                    let ids = rename(personal_ids);
                    let requested = rename(strings(&provider["modelOrder"]));
                    let mut next = provider;
                    next["personalModelIds"] = json!(ids);
                    next["modelOrder"] = json!(owned_order(inherited, &ids, &requested));
                    personal.set_config(&id, canonical_provider_config(&next));
                    personal.rename_exact(&id, &current, &next_model);
                    Ok(())
                })
                .await?;
            Ok(writer.view().await)
        })
        .await
    }

    pub async fn delete_model(&self, provider_id: &str, model_id: &str) -> Result<Value, RpcError> {
        let id = normalize_id("providerId", provider_id)?;
        let model = normalize_id("modelId", model_id)?;
        self.serialized(move |writer| async move {
            let membership = writer.membership(&id).await?;
            writer
                .update(move |personal, revision, _| {
                    check_membership(&membership, &id, revision)?;
                    if membership.inherited.contains(&model) {
                        return Err(error(format!("Built-in Model 不能删除: {id}/{model}")));
                    }
                    let Some(provider) = personal.config(&id).cloned() else {
                        return Err(error(format!("Personal Model 不存在: {id}/{model}")));
                    };
                    let ids: Vec<String> = strings(&provider["personalModelIds"]);
                    if !ids.contains(&model) {
                        return Err(error(format!("Personal Model 不存在: {id}/{model}")));
                    }
                    let remaining: Vec<String> = ids.into_iter().filter(|m| *m != model).collect();
                    let mut next = provider.clone();
                    next["personalModelIds"] = json!(remaining);
                    next["modelOrder"] = json!(owned_order(
                        &membership.inherited,
                        &remaining,
                        &strings(&provider["modelOrder"])
                    ));
                    personal.set_config(&id, canonical_provider_config(&next));
                    personal.delete_exact(&id, &model);
                    Ok(())
                })
                .await?;
            Ok(writer.view().await)
        })
        .await
    }

    pub async fn set_model_enabled(
        &self,
        provider_id: &str,
        model_id: &str,
        enabled: &Value,
    ) -> Result<Value, RpcError> {
        let id = normalize_id("providerId", provider_id)?;
        let model = normalize_id("modelId", model_id)?;
        let Some(enabled) = enabled.as_bool() else {
            return Err(error("Model enabled 必须是 boolean"));
        };
        self.serialized(move |writer| async move {
            let membership = writer.membership(&id).await?;
            writer
                .update(move |personal, revision, _| {
                    check_membership(&membership, &id, revision)?;
                    let personal_ids = personal
                        .config(&id)
                        .map(|c| strings(&c["personalModelIds"]))
                        .unwrap_or_default();
                    if !membership.inherited.contains(&model) && !personal_ids.contains(&model) {
                        return Err(error(format!("Model 不存在: {id}/{model}")));
                    }
                    let config = overlay_model_config(
                        &personal.get_exact(&id, &model).unwrap_or_else(|| json!({})),
                        &json!({"enabled": enabled}),
                    );
                    personal.set_exact(&id, &model, &config, None)?;
                    Ok(())
                })
                .await?;
            Ok(writer.view().await)
        })
        .await
    }

    pub async fn save_model_draft(&self, input: &Value) -> Result<Value, RpcError> {
        let id = normalize_id("providerId", input["providerId"].as_str().unwrap_or(""))?;
        let original = normalize_id("modelId", input["originalModelId"].as_str().unwrap_or(""))?;
        let next_model = normalize_id("modelId", input["nextModelId"].as_str().unwrap_or(""))?;
        let config = input["personalConfig"].clone();
        validate_model_config(&config, "personalConfig")
            .map_err(|message| RpcError::new("ZodError", message))?;
        let use_recommended = input["useRecommendedConfig"].as_bool();
        let based_on = input["basedOnRevision"].clone();
        self.serialized(move |writer| async move {
            let (revision, resolved, personal_revision) = writer.runtime.snapshot_with_personal().await;
            if based_on != json!(revision) {
                return Err(error(format!(
                    "Provider Settings revision conflict: expected {based_on}, current {revision}"
                )));
            }
            let membership = membership_of(&resolved, &id, personal_revision.clone())?;
            writer.update(move |personal, current_revision, _| {
                check_membership(&membership, &id, current_revision)?;
                if current_revision != personal_revision {
                    return Err(error(format!(
                        "Personal Provider Config revision conflict: expected {personal_revision}, current {current_revision}"
                    )));
                }
                let recommended = use_recommended.unwrap_or_else(|| {
                    !personal.exact_rule(&id, &original).is_some_and(|rule| rule.manual)
                });
                let inherited = &membership.inherited;
                if original != next_model && inherited.contains(&original) {
                    return Err(error(format!("Built-in Model 不能重命名: {id}/{original}")));
                }
                if original != next_model && inherited.contains(&next_model) {
                    return Err(error(format!("Model 已存在: {id}/{next_model}")));
                }
                let provider = personal.config(&id).cloned();
                let personal_ids = provider.as_ref().map(|c| strings(&c["personalModelIds"])).unwrap_or_default();
                if !inherited.contains(&original) && !personal_ids.contains(&original) {
                    return Err(error(format!("Model 不存在: {id}/{original}")));
                }
                if original != next_model && (personal_ids.contains(&next_model) || inherited.contains(&next_model)) {
                    return Err(error(format!("Model 已存在: {id}/{next_model}")));
                }
                if original != next_model {
                    let Some(provider) = provider.filter(|_| personal_ids.contains(&original)) else {
                        return Err(error(format!("Personal Model 不存在: {id}/{original}")));
                    };
                    let rename = |ids: Vec<String>| -> Vec<String> {
                        ids.into_iter().map(|m| if m == original { next_model.clone() } else { m }).collect()
                    };
                    let ids = rename(personal_ids);
                    let requested = rename(strings(&provider["modelOrder"]));
                    let mut next = provider;
                    next["personalModelIds"] = json!(ids);
                    next["modelOrder"] = json!(owned_order(inherited, &ids, &requested));
                    personal.set_config(&id, canonical_provider_config(&next));
                    personal.rename_exact(&id, &original, &next_model);
                }
                if recommended && is_structurally_empty(&canonical_model_config(&config)) {
                    personal.delete_exact(&id, &next_model);
                } else {
                    personal.set_exact(&id, &next_model, &config, Some(recommended))?;
                }
                Ok(())
            })
            .await?;
            Ok(writer.view().await)
        })
        .await
    }

    /// `resolveModelConfig`：只读预览。
    pub async fn resolve_model_config(&self, input: &Value) -> Result<Value, RpcError> {
        let provider_id = input["providerId"].as_str().unwrap_or("");
        let model_id = input["modelId"].as_str().unwrap_or("");
        let (_, resolved) = self.runtime.snapshot().await;
        let provider = resolved
            .providers
            .iter()
            .find(|provider| provider.provider_id == provider_id)
            .ok_or_else(|| error(format!("Provider 不存在: {provider_id}")))?;
        let (_, builtin) = self.runtime.builtin_snapshot().await;
        let api_type = provider.effective_config["api"]["type"]
            .as_str()
            .unwrap_or("");
        let base_url = provider.effective_config["api"]["baseUrl"]
            .as_str()
            .unwrap_or("");
        let template = provider.template_id.as_deref();
        let path = ["providers", provider_id, "models", model_id];
        let personal_value = self.read_personal_value().await;
        let personal_rules = personal_value
            .as_ref()
            .map(|value| &value["config"]["modelConfigRules"]);
        let Some(draft) = input.get("personalConfig").filter(|v| !v.is_null()) else {
            let config = effective_model_config(
                &builtin["modelConfigRules"],
                personal_rules,
                provider_id,
                template,
                model_id,
                api_type,
                base_url,
            );
            let config = canonical_model_config(&config);
            return Ok(json!({
                "inheritedConfig": config,
                "effectiveConfig": config,
                "issues": completeness_issues(&config, &path),
            }));
        };
        validate_model_config(draft, "personalConfig")
            .map_err(|message| RpcError::new("ZodError", message))?;
        let inherited = effective_model_config(
            &builtin["modelConfigRules"],
            None,
            provider_id,
            template,
            model_id,
            api_type,
            base_url,
        );
        let mut personal = personal_value
            .as_ref()
            .and_then(|value| decode(value).ok())
            .unwrap_or_default();
        let original = input["originalModelId"].as_str().unwrap_or(model_id);
        if original != model_id {
            personal.rename_exact(provider_id, original, model_id);
        }
        personal.set_exact(provider_id, model_id, draft, Some(true))?;
        let encoded = encode(&personal);
        let effective = canonical_model_config(&effective_model_config(
            &builtin["modelConfigRules"],
            Some(&encoded["config"]["modelConfigRules"]),
            provider_id,
            template,
            model_id,
            api_type,
            base_url,
        ));
        Ok(json!({
            "inheritedConfig": canonical_model_config(&inherited),
            "effectiveConfig": effective,
            "issues": completeness_issues(&effective, &path),
        }))
    }

    async fn read_personal_value(&self) -> Option<Value> {
        let bytes = tokio::fs::read(&self.file).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

fn membership_of(
    resolved: &Resolved,
    provider_id: &str,
    personal_revision: String,
) -> Result<Membership, RpcError> {
    let provider = resolved
        .providers
        .iter()
        .find(|provider| provider.provider_id == provider_id)
        .ok_or_else(|| error(format!("Provider 不存在: {provider_id}")))?;
    Ok(Membership {
        provider_id: provider_id.to_owned(),
        inherited: provider
            .models
            .iter()
            .filter(|model| model.builtin)
            .map(|model| model.model_id.clone())
            .collect(),
        personal_revision,
    })
}

fn check_membership(
    membership: &Membership,
    provider_id: &str,
    revision: &str,
) -> Result<(), RpcError> {
    if membership.provider_id != provider_id || membership.personal_revision != revision {
        return Err(error("Provider Settings membership revision conflict"));
    }
    Ok(())
}

/// `config.validateComplete(path)` 的近似：列出缺失或无效的必填叶子。
fn completeness_issues(config: &Value, path: &[&str]) -> Value {
    let mut issues = Vec::new();
    let mut missing = |field: &[&str], ok: bool| {
        if !ok {
            let mut full: Vec<&str> = path.to_vec();
            full.extend_from_slice(field);
            issues.push(json!({
                "code": "required-field-missing",
                "path": full,
                "message": format!("{} is required", field.join(".")),
            }));
        }
    };
    missing(&["enabled"], config["enabled"].is_boolean());
    let properties = &config["properties"];
    for key in [
        "requiresMfjsToolSchema",
        "supportsToolCall",
        "supportsJsonSchemaOutput",
        "supportsNativeWebSearch",
        "supportsMidConversationSystem",
    ] {
        missing(&["properties", key], properties[key].is_boolean());
    }
    missing(
        &["properties", "contextWindow"],
        properties["contextWindow"].as_u64().is_some_and(|n| n > 0),
    );
    for key in [
        "supportsText",
        "supportsImage",
        "supportsVideo",
        "supportsAudio",
        "supportsPdf",
    ] {
        missing(
            &["properties", "inputFormat", key],
            properties["inputFormat"][key].is_boolean(),
        );
    }
    missing(
        &["properties", "outputFormat", "supportsText"],
        properties["outputFormat"]["supportsText"].is_boolean(),
    );
    let specs = &config["optionSpecs"];
    missing(
        &["optionSpecs", "reasoningLevel", "values"],
        specs["reasoningLevel"]["values"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
    );
    missing(
        &["optionSpecs", "reasoningLevel", "map"],
        specs["reasoningLevel"]["map"].is_string(),
    );
    missing(
        &["optionSpecs", "maxOutputTokens", "max"],
        specs["maxOutputTokens"]["max"]
            .as_u64()
            .is_some_and(|n| n > 0),
    );
    missing(
        &["optionSpecs", "maxOutputTokens", "map"],
        specs["maxOutputTokens"]["map"].is_string(),
    );
    Value::Array(issues)
}

/// 测试连通性的执行器（由 Agent 子系统注入：同步 Account Config 后发 `provider/testModelConnectivity`）。
pub type ConnectivityTester = Arc<
    dyn Fn(Value) -> futures_util::future::BoxFuture<'static, Result<(), String>> + Send + Sync,
>;

fn ids_arg(args: &zcode_host_kit::Args, index: usize, name: &str) -> Result<Vec<String>, RpcError> {
    let value: Value = args.get(index, name)?;
    value
        .as_array()
        .filter(|items| items.iter().all(Value::is_string))
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .ok_or_else(|| RpcError::new("ZodError", format!("{name} must be an array of strings")))
}

/// provider-settings 的写成员；只登记这些方法，读成员由 `provider_settings_service` 应答。
pub fn writer_service(
    writer: PersonalWriter,
    refresh_sources: Arc<
        dyn Fn(String) -> futures_util::future::BoxFuture<'static, Result<(), String>>
            + Send
            + Sync,
    >,
    tester: Option<ConnectivityTester>,
) -> ServiceBuilder {
    macro_rules! handler {
        (|$w:ident, $args:ident| $body:expr) => {{
            let shared = writer.clone();
            move |_: zcode_host_kit::CallContext, $args: zcode_host_kit::Args| {
                let $w = shared.clone();
                async move { $body }
            }
        }};
    }
    let refresh_writer = writer.clone();
    ServiceBuilder::new("provider-settings")
        .method("refresh", move |_, args| {
            let (writer, refresh_sources) = (refresh_writer.clone(), refresh_sources.clone());
            async move {
                let reason: String = args.opt(0, "reason")?.unwrap_or_default();
                // `refreshSources("settings:<reason>")`：账号层（以及内置配置）随设置页刷新。
                let refreshed = refresh_sources(format!("settings:{reason}")).await;
                writer.runtime.reload().await;
                // 与 TS 相同：先刷新 registry，再把来源刷新失败（如 CDN 不可用）抛给调用方。
                refreshed.map_err(|message| RpcError::new("Error", message))?;
                Ok(Reply::Value(writer.view().await))
            }
        })
        .method(
            "createPersonalProvider",
            handler!(|w, args| {
                let input: Value = args.opt(0, "input")?.unwrap_or_else(|| json!({}));
                w.create_provider(&input).await.map(Reply::Value)
            }),
        )
        .method(
            "resolveModelConfig",
            handler!(|w, args| {
                let input: Value = args.get(0, "input")?;
                w.resolve_model_config(&input).await.map(Reply::Value)
            }),
        )
        .method(
            "savePersonalProviderOverlay",
            handler!(|w, args| {
                let id: String = args.get(0, "providerId")?;
                let config: Value = args.get(1, "config")?;
                let metadata: Option<Value> = args.opt(2, "metadata")?;
                w.save_provider_overlay(&id, &config, metadata.as_ref())
                    .await
                    .map(Reply::Value)
            }),
        )
        .method(
            "deletePersonalProvider",
            handler!(|w, args| {
                let id: String = args.get(0, "providerId")?;
                w.delete_provider(&id).await.map(Reply::Value)
            }),
        )
        .method(
            "reorderPersonalProviders",
            handler!(|w, args| {
                let ids = ids_arg(&args, 0, "providerIds")?;
                w.reorder_providers(ids).await.map(Reply::Value)
            }),
        )
        .method(
            "reorderPersonalModels",
            handler!(|w, args| {
                let id: String = args.get(0, "providerId")?;
                let ids = ids_arg(&args, 1, "modelIds")?;
                w.reorder_models(&id, ids).await.map(Reply::Value)
            }),
        )
        .method(
            "addPersonalModel",
            handler!(|w, args| {
                let id: String = args.get(0, "providerId")?;
                let model: String = args.get(1, "modelId")?;
                let config: Value = args.get(2, "config")?;
                let recommended: Option<bool> = args.opt(3, "useRecommendedConfig")?;
                w.add_model(&id, &model, &config, recommended)
                    .await
                    .map(Reply::Value)
            }),
        )
        .method(
            "renamePersonalModel",
            handler!(|w, args| {
                let id: String = args.get(0, "providerId")?;
                let current: String = args.get(1, "currentModelId")?;
                let next: String = args.get(2, "nextModelId")?;
                w.rename_model(&id, &current, &next).await.map(Reply::Value)
            }),
        )
        .method(
            "deletePersonalModel",
            handler!(|w, args| {
                let id: String = args.get(0, "providerId")?;
                let model: String = args.get(1, "modelId")?;
                w.delete_model(&id, &model).await.map(Reply::Value)
            }),
        )
        .method(
            "savePersonalModelDraft",
            handler!(|w, args| {
                let input: Value = args.get(0, "input")?;
                w.save_model_draft(&input).await.map(Reply::Value)
            }),
        )
        .method(
            "setPersonalModelEnabled",
            handler!(|w, args| {
                let id: String = args.get(0, "providerId")?;
                let model: String = args.get(1, "modelId")?;
                let enabled = args.raw(2).cloned().unwrap_or(Value::Null);
                w.set_model_enabled(&id, &model, &enabled)
                    .await
                    .map(Reply::Value)
            }),
        )
        .method("testModelConnectivity", move |_, args| {
            let tester = tester.clone();
            async move {
                let input: Value = args.get(0, "input")?;
                let Some(tester) = tester else {
                    return Ok(Reply::Value(json!({
                        "success": false,
                        "error": {"message": "Agent Service 尚未完成模型连通性测试装配"},
                    })));
                };
                Ok(Reply::Value(match tester(input).await {
                    Ok(()) => json!({"success": true}),
                    Err(message) => json!({"success": false, "error": {"message": message}}),
                }))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_path() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../../config/provider/zcode-builtin.json"
        ))
    }

    #[tokio::test]
    async fn create_add_rename_and_delete_round_trip_the_personal_file() {
        let dir = tempfile::tempdir().unwrap();
        let personal = dir.path().join("provider_config.json");
        let runtime = ProviderRuntime::new(
            &zcode_host_base::testing::spawner(),
            Some(builtin_path()),
            personal.clone(),
        );
        let writer = PersonalWriter::new(runtime.clone());
        let created = writer
            .create_provider(&json!({"initialConfig": {"api": {"type": "openai-chat-completions", "baseUrl": "https://api.example.com/v1"}, "access": {"type": "api-key", "apiKey": "sk-x"}}}))
            .await
            .unwrap();
        assert_eq!(created["providerId"], "new-provider");
        writer
            .add_model("new-provider", "m1", &json!({}), None)
            .await
            .unwrap();
        writer
            .add_model(
                "new-provider",
                "m2",
                &json!({"properties": {"contextWindow": 1000}}),
                None,
            )
            .await
            .unwrap();
        assert!(
            writer
                .add_model("new-provider", "m1", &json!({}), None)
                .await
                .is_err()
        );
        writer
            .rename_model("new-provider", "m1", "m1b")
            .await
            .unwrap();
        writer
            .reorder_models("new-provider", vec!["m2".into(), "m1b".into()])
            .await
            .unwrap();
        writer
            .set_model_enabled("new-provider", "m2", &json!(false))
            .await
            .unwrap();
        let text = std::fs::read_to_string(&personal).unwrap();
        assert!(!text.ends_with('\n'));
        let file: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            file,
            json!({"schemaVersion": 1, "config": {
                "providerOrder": ["new-provider"],
                "providerConfigRules": {"providerRules": [{
                    "providerId": "new-provider",
                    "providerName": "new-provider",
                    "config": {
                        "group": "standard-personal",
                        "access": {"type": "api-key", "apiKey": "sk-x"},
                        "api": {"type": "openai-chat-completions", "baseUrl": "https://api.example.com/v1"},
                        "personalModelIds": ["m1b", "m2"],
                        "modelOrder": ["m2", "m1b"],
                    },
                }]},
                "modelConfigRules": {
                    "providerModelRules": [
                        {"modelId": "m1b", "config": {"enabled": true}, "providerId": "new-provider"},
                        {"modelId": "m2", "config": {"enabled": false, "properties": {"contextWindow": 1000}}, "providerId": "new-provider"},
                    ],
                    "manualProviderModelRules": [],
                },
            }})
        );
        // 第二个同名 provider 自动加后缀。
        let second = writer.create_provider(&json!({})).await.unwrap();
        assert_eq!(second["providerId"], "new-provider-2");
        let view = writer
            .reorder_providers(vec!["new-provider-2".into()])
            .await
            .unwrap();
        assert_eq!(
            view["providerOrder"],
            json!(["new-provider-2", "new-provider"])
        );
        assert!(
            view["providers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["providerName"] == "new-provider 2")
        );
        let renamed = writer
            .save_provider_overlay(
                "new-provider-2",
                &json!({}),
                Some(&json!({"providerName": "New-Provider"})),
            )
            .await;
        assert_eq!(
            renamed.unwrap_err().message,
            "Provider 名称已存在: New-Provider"
        );
        writer.delete_model("new-provider", "m2").await.unwrap();
        writer.delete_provider("new-provider").await.unwrap();
        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(&personal).unwrap()).unwrap();
        assert_eq!(file["config"]["providerOrder"], json!(["new-provider-2"]));
        assert_eq!(
            file["config"]["modelConfigRules"]["providerModelRules"],
            json!([])
        );
    }

    #[tokio::test]
    async fn account_providers_reject_access_and_draft_checks_revision() {
        let dir = tempfile::tempdir().unwrap();
        let personal = dir.path().join("provider_config.json");
        let runtime = ProviderRuntime::new(
            &zcode_host_base::testing::spawner(),
            Some(builtin_path()),
            personal.clone(),
        );
        let writer = PersonalWriter::new(runtime.clone());
        let error = writer
            .save_provider_overlay(
                "account:zai-start-plan",
                &json!({"access": {"type": "api-key"}}),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("固定 Account Provider 的 Access"));
        let (revision, _) = runtime.snapshot().await;
        let stale = writer
            .save_model_draft(&json!({"providerId": "account:zai-start-plan", "originalModelId": "GLM-5.3-Flash", "nextModelId": "GLM-5.3-Flash", "personalConfig": {}, "basedOnRevision": revision + 5}))
            .await
            .unwrap_err();
        assert!(
            stale
                .message
                .starts_with("Provider Settings revision conflict")
        );
        writer
            .save_model_draft(&json!({"providerId": "account:zai-start-plan", "originalModelId": "GLM-5.3-Flash", "nextModelId": "GLM-5.3-Flash", "personalConfig": {"enabled": false}, "basedOnRevision": revision}))
            .await
            .unwrap();
        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(&personal).unwrap()).unwrap();
        assert_eq!(
            file["config"]["modelConfigRules"]["providerModelRules"],
            json!([{"modelId": "GLM-5.3-Flash", "config": {"enabled": false}, "providerId": "account:zai-start-plan"}])
        );
        // 手动模式缺可编辑叶子：拒绝写入。
        let (revision, _) = runtime.snapshot().await;
        let manual = writer
            .save_model_draft(&json!({"providerId": "account:zai-start-plan", "originalModelId": "GLM-5.3-Flash", "nextModelId": "GLM-5.3-Flash", "personalConfig": {"enabled": true}, "basedOnRevision": revision, "useRecommendedConfig": false}))
            .await;
        assert!(manual.is_err());
    }
}
