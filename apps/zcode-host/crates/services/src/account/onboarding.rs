//! `onboarding-record` channel（packages/services/src/onboarding/onboardingRecordService.ts）。
//!
//! 设备级文件 `<appConfigDir>/onboarding-record.json`（跟随 dataBaseDir），写入用
//! `atomicWriteText`（目录锁 + 原子替换），进程内写入串行。userId 来自当前登录 profile。
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder, log};

use crate::store::atomic_write_text;

/// `appSettingsOccupationEnum`：记录里的职业不在枚举内时回填 other。
const OCCUPATIONS: [&str; 14] = [
    "office",
    "developer",
    "independent",
    "infrastructure",
    "product",
    "design",
    "student",
    "creator",
    "operations",
    "marketing",
    "finance",
    "accounting",
    "legal",
    "other",
];

pub type UserIdLoader = Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>;
pub type LocalTaskProbe = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;

/// `new Date().toISOString()`。
pub fn iso_now() -> String {
    iso_from_ms(super::oauth_adapters::now_ms())
}

pub fn iso_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant 的 civil_from_days。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn nullable_string_min1(value: &Value) -> Option<Value> {
    match value {
        Value::Null => Some(Value::Null),
        Value::String(text) if !text.is_empty() => Some(value.clone()),
        _ => None,
    }
}

fn nullable_bool(value: &Value) -> Option<Value> {
    match value {
        Value::Null | Value::Bool(_) => Some(value.clone()),
        _ => None,
    }
}

/// `onboardingRecordEntrySchema.parse`：按 schema 键序输出，多余键剥离。
fn parse_entry(value: &Value) -> Option<Value> {
    let mut entry = Map::new();
    entry.insert("userId".into(), nullable_string_min1(&value["userId"])?);
    entry.insert(
        "occupation".into(),
        nullable_string_min1(&value["occupation"])?,
    );
    let mode = &value["interfaceMode"];
    if !(mode.is_null() || mode == "coding" || mode == "office") {
        return None;
    }
    entry.insert("interfaceMode".into(), mode.clone());
    entry.insert(
        "memoryEnabled".into(),
        nullable_bool(value.get("memoryEnabled")?)?,
    );
    entry.insert(
        "proactiveSuggestionsEnabled".into(),
        nullable_bool(value.get("proactiveSuggestionsEnabled")?)?,
    );
    let completed = value["completedAt"].as_str().filter(|s| !s.is_empty())?;
    entry.insert("completedAt".into(), json!(completed));
    if value["uploadState"] != "pending" {
        return None;
    }
    entry.insert("uploadState".into(), json!("pending"));
    // zod 的 nullable 不接受缺失键（undefined）；interfaceMode / occupation 缺失时同样失败。
    if value.get("occupation").is_none() || value.get("interfaceMode").is_none() {
        return None;
    }
    Some(Value::Object(entry))
}

fn parse_decision(value: &Value) -> Option<Value> {
    let status = value["status"].as_str()?;
    let reason = value["reason"].as_str()?;
    if !matches!(status, "dismissed" | "existing_local_user")
        || !matches!(reason, "user_closed" | "existing_local_task")
        || value.get("userId").is_none()
    {
        return None;
    }
    Some(json!({
        "userId": nullable_string_min1(&value["userId"])?,
        "status": status,
        "reason": reason,
        "decidedAt": value["decidedAt"].as_str().filter(|s| !s.is_empty())?,
    }))
}

/// `onboardingRecordFileSchema`：v1 补空 decisions 升为 v2。
fn parse_file(value: &Value) -> Option<Value> {
    let device = value["deviceMid"].as_str().filter(|s| !s.is_empty())?;
    let entries: Vec<Value> = value["entries"]
        .as_array()?
        .iter()
        .map(parse_entry)
        .collect::<Option<_>>()?;
    let decisions: Vec<Value> = match value["version"].as_f64() {
        Some(1.0) => Vec::new(),
        Some(2.0) => value["decisions"]
            .as_array()?
            .iter()
            .map(parse_decision)
            .collect::<Option<_>>()?,
        _ => return None,
    };
    Some(json!({"version": 2, "deviceMid": device, "entries": entries, "decisions": decisions}))
}

#[derive(Clone)]
pub struct OnboardingRecords {
    file: PathBuf,
    load_user_id: UserIdLoader,
    has_local_task: LocalTaskProbe,
    queue: Arc<tokio::sync::Mutex<()>>,
}

fn user_matches(item: &Value, user: &Option<String>) -> bool {
    match user {
        Some(user) => item["userId"].as_str() == Some(user.as_str()),
        None => item["userId"].is_null(),
    }
}

fn has_identity_record(file: &Value, user: &Option<String>) -> bool {
    ["entries", "decisions"].iter().any(|key| {
        file[*key]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| user_matches(item, user)))
    })
}

fn invalid(message: &str) -> RpcError {
    RpcError::new("ZodError", message.to_owned())
}

impl OnboardingRecords {
    pub fn new(
        app_config_dir: PathBuf,
        load_user_id: UserIdLoader,
        has_local_task: LocalTaskProbe,
    ) -> Self {
        Self {
            file: app_config_dir.join("onboarding-record.json"),
            load_user_id,
            has_local_task,
            queue: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn read(&self) -> Option<Value> {
        let raw = match tokio::fs::read_to_string(&self.file).await {
            Ok(raw) => raw,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warn(format!(
                        "[onboardingRecordService] read onboarding record failed: {error}"
                    ));
                }
                return None;
            }
        };
        let parsed = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|value| parse_file(&value));
        if parsed.is_none() {
            log::warn(
                "[onboardingRecordService] invalid onboarding record json, treating as missing",
            );
        }
        parsed
    }

    async fn write(&self, file: &Value) -> Result<(), RpcError> {
        let text = serde_json::to_string_pretty(file).unwrap_or_default();
        atomic_write_text(&self.file, &text, true)
            .await
            .map_err(|error| RpcError::new("Error", format!("{error:#}")))
    }

    fn new_file(device_mid: &str) -> Value {
        json!({"version": 2, "deviceMid": device_mid, "entries": [], "decisions": []})
    }

    pub async fn append_record(&self, device_mid: &str, entry: &Value) -> Result<(), RpcError> {
        let user = (self.load_user_id)().await;
        let _queue = self.queue.lock().await;
        let mut file = match self.read().await {
            Some(existing) => {
                if existing["deviceMid"] != device_mid {
                    log::warn(format!(
                        "[onboardingRecordService] deviceMid mismatch, keep existing: {} incoming: {device_mid}",
                        existing["deviceMid"]
                    ));
                }
                existing
            }
            None => Self::new_file(device_mid),
        };
        let mut record = Map::new();
        record.insert("userId".into(), json!(user));
        if let Some(input) = entry.as_object() {
            for (key, value) in input {
                record.insert(key.clone(), value.clone());
            }
        }
        record.insert("uploadState".into(), json!("pending"));
        let validated = parse_entry(&Value::Object(record))
            .ok_or_else(|| invalid("invalid onboarding record entry"))?;
        let entries = file["entries"].as_array_mut().expect("parsed entries");
        match entries.iter().position(|item| user_matches(item, &user)) {
            Some(index) => entries[index] = validated,
            None => entries.push(validated),
        }
        if let Some(decisions) = file["decisions"].as_array_mut() {
            decisions.retain(|decision| !user_matches(decision, &user));
        }
        self.write(&file).await
    }

    pub async fn claim_anonymous_record(&self) -> Result<(), RpcError> {
        let Some(user) = (self.load_user_id)().await else {
            return Ok(());
        };
        let _queue = self.queue.lock().await;
        let Some(mut file) = self.read().await else {
            return Ok(());
        };
        let identity = Some(user.clone());
        if has_identity_record(&file, &identity) {
            return Ok(());
        }
        for key in ["entries", "decisions"] {
            let items = file[key].as_array_mut().expect("parsed list");
            if let Some(item) = items.iter_mut().rev().find(|item| item["userId"].is_null()) {
                item["userId"] = json!(user);
                return self.write(&file).await;
            }
        }
        Ok(())
    }

    pub async fn should_onboard(&self, device_mid: &str) -> Result<bool, RpcError> {
        let user = (self.load_user_id)().await;
        if self
            .read()
            .await
            .is_some_and(|file| has_identity_record(&file, &user))
        {
            return Ok(false);
        }
        if !(self.has_local_task)().await {
            return Ok(true);
        }
        let _queue = self.queue.lock().await;
        let mut current = self
            .read()
            .await
            .unwrap_or_else(|| Self::new_file(device_mid));
        if has_identity_record(&current, &user) {
            return Ok(false);
        }
        current["decisions"]
            .as_array_mut()
            .expect("parsed decisions")
            .push(json!({
                "userId": user,
                "status": "existing_local_user",
                "reason": "existing_local_task",
                "decidedAt": iso_now(),
            }));
        self.write(&current).await?;
        Ok(false)
    }

    pub async fn dismiss_onboarding(&self, device_mid: &str) -> Result<(), RpcError> {
        let user = (self.load_user_id)().await;
        let _queue = self.queue.lock().await;
        let mut file = self
            .read()
            .await
            .unwrap_or_else(|| Self::new_file(device_mid));
        if file["entries"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| user_matches(entry, &user)))
        {
            return Ok(());
        }
        let decision = json!({
            "userId": user,
            "status": "dismissed",
            "reason": "user_closed",
            "decidedAt": iso_now(),
        });
        let decisions = file["decisions"].as_array_mut().expect("parsed decisions");
        match decisions.iter().position(|item| user_matches(item, &user)) {
            Some(index) => decisions[index] = decision,
            None => decisions.push(decision),
        }
        self.write(&file).await
    }

    async fn latest_entry(&self) -> Option<Value> {
        let user = (self.load_user_id)().await;
        let file = self.read().await?;
        file["entries"]
            .as_array()?
            .iter()
            .rev()
            .find(|entry| user_matches(entry, &user))
            .cloned()
    }

    pub async fn sync_settings_from_record(&self) -> Value {
        let Some(latest) = self.latest_entry().await else {
            return Value::Null;
        };
        let occupation = latest["occupation"]
            .as_str()
            .filter(|value| !value.is_empty() && OCCUPATIONS.contains(value))
            .unwrap_or("other");
        json!({
            "onboardingOccupation": occupation,
            "proactiveSuggestionsEnabled": latest["proactiveSuggestionsEnabled"].as_bool().unwrap_or(false),
            "memoryEnabled": latest["memoryEnabled"].as_bool().unwrap_or(false),
        })
    }

    pub async fn update_record_preferences(&self, patch: &Value) -> Result<(), RpcError> {
        let user = (self.load_user_id)().await;
        let _queue = self.queue.lock().await;
        let Some(mut file) = self.read().await else {
            return Ok(());
        };
        let entries = file["entries"].as_array_mut().expect("parsed entries");
        let Some(index) = entries.iter().rposition(|entry| user_matches(entry, &user)) else {
            return Ok(());
        };
        let mut merged = entries[index].as_object().cloned().unwrap_or_default();
        if let Some(patch) = patch.as_object() {
            for (key, value) in patch {
                merged.insert(key.clone(), value.clone());
            }
        }
        entries[index] = parse_entry(&Value::Object(merged))
            .ok_or_else(|| invalid("invalid onboarding record preferences"))?;
        self.write(&file).await
    }

    pub async fn clear_records(&self) -> Result<(), RpcError> {
        let _queue = self.queue.lock().await;
        match tokio::fs::remove_file(&self.file).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RpcError::new("Error", error.to_string())),
        }
    }
}

pub fn service(records: OnboardingRecords) -> ServiceBuilder {
    macro_rules! handler {
        (|$records:ident, $args:ident| $body:expr) => {{
            let shared = records.clone();
            move |_: zcode_host_kit::CallContext, $args: zcode_host_kit::Args| {
                let $records = shared.clone();
                async move { $body }
            }
        }};
    }
    ServiceBuilder::new("onboarding-record")
        .method(
            "appendRecord",
            handler!(|records, args| {
                let device: String = args.get(0, "deviceMid")?;
                let entry: Value = args.get(1, "entry")?;
                records
                    .append_record(&device, &entry)
                    .await
                    .map(|()| Reply::Undefined)
            }),
        )
        .method(
            "shouldOnboard",
            handler!(|records, args| {
                let device: String = args.get(0, "deviceMid")?;
                records
                    .should_onboard(&device)
                    .await
                    .map(|value| Reply::Value(json!(value)))
            }),
        )
        .method(
            "dismissOnboarding",
            handler!(|records, args| {
                let device: String = args.get(0, "deviceMid")?;
                records
                    .dismiss_onboarding(&device)
                    .await
                    .map(|()| Reply::Undefined)
            }),
        )
        .method(
            "claimAnonymousRecord",
            handler!(|records, _args| records
                .claim_anonymous_record()
                .await
                .map(|()| Reply::Undefined)),
        )
        .method(
            "getLatestEntry",
            handler!(|records, _args| Ok(Reply::Value(
                records.latest_entry().await.unwrap_or(Value::Null)
            ))),
        )
        .method(
            "syncSettingsFromRecord",
            handler!(|records, _args| Ok(Reply::Value(records.sync_settings_from_record().await))),
        )
        .method(
            "updateRecordPreferences",
            handler!(|records, args| {
                let patch: Value = args.opt(0, "patch")?.unwrap_or_else(|| json!({}));
                records
                    .update_record_preferences(&patch)
                    .await
                    .map(|()| Reply::Undefined)
            }),
        )
        .method(
            "getRecords",
            handler!(|records, _args| Ok(Reply::Value(
                records.read().await.unwrap_or(Value::Null)
            ))),
        )
        .method(
            "clearRecords",
            handler!(|records, _args| records.clear_records().await.map(|()| Reply::Undefined)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn records(
        dir: &std::path::Path,
        user: Arc<Mutex<Option<String>>>,
        has_task: bool,
    ) -> OnboardingRecords {
        OnboardingRecords::new(
            dir.to_path_buf(),
            Arc::new(move || {
                let user = user.lock().unwrap().clone();
                Box::pin(async move { user })
            }),
            Arc::new(move || Box::pin(async move { has_task })),
        )
    }

    fn entry() -> Value {
        json!({"occupation": "developer", "interfaceMode": "coding", "memoryEnabled": true, "proactiveSuggestionsEnabled": null, "completedAt": "2026-01-01T00:00:00.000Z"})
    }

    #[tokio::test]
    async fn append_claim_and_sync_follow_node_rules() {
        let dir = tempfile::tempdir().unwrap();
        let user = Arc::new(Mutex::new(None));
        let records = records(dir.path(), user.clone(), false);
        assert!(records.should_onboard("mid").await.unwrap());
        records.dismiss_onboarding("mid").await.unwrap();
        assert!(!records.should_onboard("mid").await.unwrap());
        records.append_record("mid", &entry()).await.unwrap();
        let file = records.read().await.unwrap();
        assert_eq!(file["decisions"], json!([]));
        assert_eq!(
            file["entries"][0],
            json!({"userId": null, "occupation": "developer", "interfaceMode": "coding", "memoryEnabled": true, "proactiveSuggestionsEnabled": null, "completedAt": "2026-01-01T00:00:00.000Z", "uploadState": "pending"})
        );
        // 登录后认领匿名记录。
        *user.lock().unwrap() = Some("u1".into());
        records.claim_anonymous_record().await.unwrap();
        assert_eq!(records.read().await.unwrap()["entries"][0]["userId"], "u1");
        assert_eq!(
            records.sync_settings_from_record().await,
            json!({"onboardingOccupation": "developer", "proactiveSuggestionsEnabled": false, "memoryEnabled": true})
        );
        records
            .update_record_preferences(&json!({"memoryEnabled": false}))
            .await
            .unwrap();
        assert_eq!(
            records.latest_entry().await.unwrap()["memoryEnabled"],
            false
        );
        let text = std::fs::read_to_string(dir.path().join("onboarding-record.json")).unwrap();
        assert!(text.starts_with("{\n  \"version\": 2,\n  \"deviceMid\": \"mid\""));
        assert!(
            records
                .append_record("mid", &json!({"occupation": "x"}))
                .await
                .is_err()
        );
        records.clear_records().await.unwrap();
        assert!(records.read().await.is_none());
    }

    #[tokio::test]
    async fn existing_local_task_records_decision_and_v1_files_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("onboarding-record.json"),
            r#"{"version":1,"deviceMid":"old","entries":[]}"#,
        )
        .unwrap();
        let records = records(dir.path(), Arc::new(Mutex::new(Some("u2".into()))), true);
        assert!(!records.should_onboard("mid").await.unwrap());
        let file = records.read().await.unwrap();
        assert_eq!(file["deviceMid"], "old");
        assert_eq!(file["decisions"][0]["status"], "existing_local_user");
        assert_eq!(file["decisions"][0]["userId"], "u2");
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(1_760_000_000_123), "2025-10-09T08:53:20.123Z");
    }
}
