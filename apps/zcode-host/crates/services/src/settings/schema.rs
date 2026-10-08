//! `AppSettings` 的读写语义（packages/shared/src/validationAppSettings.ts、
//! packages/services/src/setting/normalizeSettingsPatch.ts、legacyAccountConnectionSettings.ts）。
//!
//! 与 zod 版保持同一组规则：预处理迁移按原顺序执行；顶层未知字段被丢弃；任何一个字段不合法
//! 整份文件回退默认值（三个容错字段除外）；输出字段顺序等于 schema 声明顺序。
//! 原则：Rust 可以比 TS 宽松，但绝不能更严格——更严格会把用户的合法设置当成坏文件回退默认，
//! 下一次写入就把它们抹掉。深层嵌套（远端目标的资源包 id 等）因此只校验到结构骨架。
use serde_json::{Map, Value, json};

const OCCUPATIONS: &[&str] = &[
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
const LOCALES: &[&str] = &["zh-CN", "en-US"];
const PROVIDER_FAMILIES: &[&str] = &["zai", "bigmodel"];

type Validator = fn(&Value) -> Option<Value>;

#[derive(Clone, Copy)]
enum Default {
    None,
    Value(fn() -> Value),
}

struct Field {
    name: &'static str,
    validate: Validator,
    default: Default,
    /// `.nullish()`：存储的 null 原样返回。
    nullable: bool,
}

const fn field(name: &'static str, validate: Validator, default: Default) -> Field {
    Field {
        name,
        validate,
        default,
        nullable: false,
    }
}

fn trimmed_non_empty(value: &Value) -> Option<Value> {
    let text = value.as_str()?.trim();
    (!text.is_empty()).then(|| Value::String(text.to_owned()))
}

fn boolean(value: &Value) -> Option<Value> {
    value.is_boolean().then(|| value.clone())
}

fn integer(value: &Value) -> Option<i64> {
    if let Some(value) = value.as_i64() {
        return Some(value);
    }
    let value = value.as_f64()?;
    (value.is_finite() && value.fract() == 0.0 && value.abs() <= 9.007_199_254_740_991e15)
        .then_some(value as i64)
}

fn int_in(value: &Value, min: i64, max: i64) -> Option<Value> {
    integer(value)
        .filter(|value| (min..=max).contains(value))
        .map(Value::from)
}

fn one_of(value: &Value, allowed: &[&str]) -> Option<Value> {
    value
        .as_str()
        .filter(|text| allowed.contains(text))
        .map(|text| Value::String(text.to_owned()))
}

fn string_array(value: &Value) -> Option<Value> {
    let items = value.as_array()?;
    items.iter().all(Value::is_string).then(|| value.clone())
}

fn object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

/// 取对象里的可选字段：缺失视为未提供；存在但不合法返回 Err（整体失败）。
fn optional(
    source: &Map<String, Value>,
    key: &str,
    validate: impl Fn(&Value) -> Option<Value>,
    out: &mut Map<String, Value>,
) -> Result<(), ()> {
    match source.get(key) {
        None => Ok(()),
        Some(value) => {
            out.insert(key.to_owned(), validate(value).ok_or(())?);
            Ok(())
        }
    }
}

fn required(
    source: &Map<String, Value>,
    key: &str,
    validate: impl Fn(&Value) -> Option<Value>,
    out: &mut Map<String, Value>,
) -> Result<(), ()> {
    out.insert(
        key.to_owned(),
        validate(source.get(key).ok_or(())?).ok_or(())?,
    );
    Ok(())
}

fn shortcut_bindings(value: &Value) -> Option<Value> {
    object(value)?
        .values()
        .all(|item| string_array(item).is_some())
        .then(|| value.clone())
}

fn string_record(value: &Value) -> Option<Value> {
    object(value)?
        .values()
        .all(Value::is_string)
        .then(|| value.clone())
}

fn integrated_terminal_shell(value: &Value) -> Option<Value> {
    let source = object(value)?;
    match source.get("mode")?.as_str()? {
        "auto" => Some(json!({"mode": "auto"})),
        "shell" => {
            let mut out = Map::new();
            out.insert("mode".into(), json!("shell"));
            required(
                source,
                "dialect",
                |v| one_of(v, &["cmd", "git-bash"]),
                &mut out,
            )
            .ok()?;
            for key in ["id", "label", "path"] {
                required(source, key, trimmed_non_empty, &mut out).ok()?;
            }
            Some(Value::Object(out))
        }
        _ => None,
    }
}

fn viewport_preference(value: &Value) -> Option<Value> {
    // embeddedBrowserViewportPreferenceSchema 是 strict：多余字段也算非法。
    let source = object(value)?;
    if source.len() != 3 {
        return None;
    }
    let viewport = object(source.get("viewport")?)?;
    if viewport.len() != 2 {
        return None;
    }
    Some(json!({
        "mode": one_of(source.get("mode")?, &["normal", "responsive"])?,
        "viewport": {
            "width": int_in(viewport.get("width")?, 320, 3840)?,
            "height": int_in(viewport.get("height")?, 320, 2160)?,
        },
        "zoom": one_of(source.get("zoom")?, &["fit", "50", "75", "100", "125", "150", "200"])?,
    }))
}

fn default_viewport_preference() -> Value {
    json!({"mode": "normal", "viewport": {"width": 393, "height": 852}, "zoom": "fit"})
}

fn desktop_window_size(value: &Value) -> Option<Value> {
    let source = object(value)?;
    Some(json!({
        "width": int_in(source.get("width")?, 480, i64::MAX)?,
        "height": int_in(source.get("height")?, 640, i64::MAX)?,
        "maximized": boolean(source.get("maximized")?)?,
    }))
}

fn connection_selection(value: &Value) -> Option<Value> {
    // 每个变体都是 strict object。
    let source = object(value)?;
    match source.get("kind")?.as_str()? {
        kind @ ("start-plan" | "individual-coding-plan") => {
            (source.len() == 1).then(|| json!({"kind": kind}))
        }
        "team-coding-plan" => {
            if source.len() != 4 {
                return None;
            }
            let mut out = Map::new();
            out.insert("kind".into(), json!("team-coding-plan"));
            for key in ["productId", "organizationId", "projectId"] {
                required(source, key, trimmed_non_empty, &mut out).ok()?;
            }
            Some(Value::Object(out))
        }
        _ => None,
    }
}

fn connection_selections(value: &Value) -> Option<Value> {
    // 未知 family 键被丢弃，已知键的值必须合法。
    let source = object(value)?;
    let mut out = Map::new();
    for family in PROVIDER_FAMILIES {
        optional(source, family, connection_selection, &mut out).ok()?;
    }
    Some(Value::Object(out))
}

fn remote_target(value: &Value) -> Option<Value> {
    let source = object(value)?;
    let mut out = Map::new();
    match source.get("kind")?.as_str()? {
        "ssh" => {
            out.insert("kind".into(), json!("ssh"));
            required(source, "host", trimmed_non_empty, &mut out).ok()?;
            optional(source, "port", |v| int_in(v, 1, 65535), &mut out).ok()?;
            required(source, "username", trimmed_non_empty, &mut out).ok()?;
            optional(source, "sshConfigAlias", trimmed_non_empty, &mut out).ok()?;
            optional(
                source,
                "privateKeyPath",
                |v| v.is_string().then(|| v.clone()),
                &mut out,
            )
            .ok()?;
            optional(
                source,
                "assetInstallMode",
                |v| one_of(v, &["local-download-upload", "remote-download"]),
                &mut out,
            )
            .ok()?;
            optional(
                source,
                "resourcePackages",
                |v| {
                    let packages = object(v)?;
                    let mut out = Map::new();
                    optional(packages, "selectedPackageIds", string_array, &mut out).ok()?;
                    Some(Value::Object(out))
                },
                &mut out,
            )
            .ok()?;
            optional(source, "passwordCredentialKey", trimmed_non_empty, &mut out).ok()?;
            optional(
                source,
                "privateKeyPassphraseCredentialKey",
                trimmed_non_empty,
                &mut out,
            )
            .ok()?;
        }
        "wsl" => {
            out.insert("kind".into(), json!("wsl"));
            optional(
                source,
                "distro",
                |v| v.is_string().then(|| v.clone()),
                &mut out,
            )
            .ok()?;
            optional(source, "user", wsl_user, &mut out).ok()?;
        }
        "docker" => {
            out.insert("kind".into(), json!("docker"));
            required(source, "container", trimmed_non_empty, &mut out).ok()?;
        }
        _ => return None,
    }
    Some(Value::Object(out))
}

fn wsl_user(value: &Value) -> Option<Value> {
    let user = value.as_str()?.trim();
    let valid = user.is_empty()
        || (user.chars().count() <= 64
            && !user.chars().any(char::is_control)
            && !user.contains([':', '/', '\\']));
    (valid && user.chars().count() <= 64).then(|| Value::String(user.to_owned()))
}

fn workspace_session_entry(value: &Value) -> Option<Value> {
    let source = object(value)?;
    let mut out = Map::new();
    match source.get("kind")?.as_str()? {
        "local" => {
            out.insert("kind".into(), json!("local"));
            required(source, "workspacePath", trimmed_non_empty, &mut out).ok()?;
            let purpose = match source.get("workspacePurpose") {
                None => json!("project"),
                Some(value) => one_of(value, &["project", "conversation"])?,
            };
            out.insert("workspacePurpose".into(), purpose);
        }
        "remote" => {
            out.insert("kind".into(), json!("remote"));
            required(source, "workspacePath", trimmed_non_empty, &mut out).ok()?;
            optional(source, "localWorkspacePath", trimmed_non_empty, &mut out).ok()?;
            optional(source, "workspaceIdentity", trimmed_non_empty, &mut out).ok()?;
            required(source, "target", remote_target, &mut out).ok()?;
            required(source, "lastOpenedAt", |v| int_in(v, 0, i64::MAX), &mut out).ok()?;
            required(
                source,
                "lastConnectionStatus",
                |v| one_of(v, &["connected", "failed"]),
                &mut out,
            )
            .ok()?;
            optional(
                source,
                "lastConnectionError",
                |v| v.is_string().then(|| v.clone()),
                &mut out,
            )
            .ok()?;
        }
        _ => return None,
    }
    Some(Value::Object(out))
}

fn workspace_session(value: &Value) -> Option<Value> {
    value
        .as_array()?
        .iter()
        .map(workspace_session_entry)
        .collect::<Option<Vec<_>>>()
        .map(Value::Array)
}

fn release_notes(value: &Value) -> Option<Value> {
    let source = object(value)?;
    let mut out = Map::new();
    for key in ["version", "title", "markdown"] {
        required(source, key, trimmed_non_empty, &mut out).ok()?;
    }
    optional(source, "releaseDate", trimmed_non_empty, &mut out).ok()?;
    optional(
        source,
        "releaseNotesByLocale",
        |v| {
            let locales = object(v)?;
            let mut out = Map::new();
            for (locale, notes) in locales {
                if !LOCALES.contains(&locale.as_str()) {
                    return None;
                }
                let notes = object(notes)?;
                let mut entry = Map::new();
                required(notes, "title", trimmed_non_empty, &mut entry).ok()?;
                required(notes, "markdown", trimmed_non_empty, &mut entry).ok()?;
                out.insert(locale.clone(), Value::Object(entry));
            }
            Some(Value::Object(out))
        },
        &mut out,
    )
    .ok()?;
    Some(Value::Object(out))
}

fn skipped_update_versions(value: &Value) -> Option<Value> {
    let source = object(value)?;
    let mut out = Map::new();
    for (channel, version) in source {
        if !["stable", "preview"].contains(&channel.as_str()) {
            return None;
        }
        out.insert(channel.clone(), trimmed_non_empty(version)?);
    }
    Some(Value::Object(out))
}

/// `normalizeZCodeEndpointOrigin`：http(s) URL 的 origin。
pub fn endpoint_origin(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let url = url::Url::parse(trimmed).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

fn endpoint_origin_value(value: &Value) -> Option<Value> {
    value.is_string().then(|| value.clone())
}

macro_rules! constant {
    ($value:expr) => {
        Default::Value(|| json!($value))
    };
}

const FIELDS: &[Field] = &[
    field(
        "recentProjects",
        string_array,
        Default::Value(|| Value::Array(Vec::new())),
    ),
    field("locale", |v| one_of(v, LOCALES), constant!("zh-CN")),
    field("shortcutBindings", shortcut_bindings, Default::None),
    field(
        "localePreference",
        |v| one_of(v, &["system", "zh-CN", "en-US"]),
        constant!("system"),
    ),
    field("terminalInheritSystemProfile", boolean, constant!(true)),
    field("terminalFontFamily", trimmed_non_empty, Default::None),
    field(
        "integratedTerminalShell",
        integrated_terminal_shell,
        Default::None,
    ),
    field("httpProxy", trimmed_non_empty, Default::None),
    field("httpProxyNoProxy", trimmed_non_empty, Default::None),
    field("httpProxyCaCertPath", trimmed_non_empty, Default::None),
    field(
        "embeddedBrowserAllowInsecureCertificates",
        boolean,
        constant!(false),
    ),
    field(
        "embeddedBrowserViewportPreference",
        viewport_preference,
        Default::Value(default_viewport_preference),
    ),
    field("computerUseComposerEntryHidden", boolean, constant!(true)),
    field("taskAutoArchiveEnabled", boolean, constant!(false)),
    field(
        "taskAutoArchiveOlderThanDays",
        |v| int_in(v, 1, 365),
        constant!(7),
    ),
    field("closeToTrayOnWindows", boolean, constant!(true)),
    field(
        "closeToTrayOnWindowsMigrationInitialized",
        boolean,
        constant!(true),
    ),
    field("keepAwakeWhileRunning", boolean, constant!(false)),
    field("desktopZoomLevel", |v| int_in(v, -3, 5), Default::None),
    field("desktopWindowSize", desktop_window_size, Default::None),
    field(
        "desktopChromiumHardwareAccelerationEnabled",
        boolean,
        constant!(true),
    ),
    field("messageStreamShowReasoning", boolean, constant!(true)),
    field(
        "messageStreamShowReasoningMigrationInitialized",
        boolean,
        constant!(true),
    ),
    field("messageStreamShowTodos", boolean, constant!(false)),
    field("toolGroupingExploreEnabled", boolean, constant!(true)),
    field("toolGroupingTerminalEnabled", boolean, constant!(true)),
    field("toolGroupingChangesEnabled", boolean, constant!(false)),
    field(
        "zcodeInteractionBehavior",
        |v| one_of(v, &["queue", "guide"]),
        constant!("queue"),
    ),
    field(
        "askUserQuestionAutoResolutionEnabled",
        boolean,
        constant!(true),
    ),
    field("modelIoFullRetentionEnabled", boolean, constant!(false)),
    field(
        "startPlanRecommendationDismissed",
        boolean,
        constant!(false),
    ),
    field(
        "providerFamilyConnectionSelections",
        connection_selections,
        Default::Value(|| Value::Object(Map::new())),
    ),
    field(
        "providerFamilyDomain",
        |v| one_of(v, PROVIDER_FAMILIES),
        Default::None,
    ),
    field(
        "providerFamilyDomainUpdatedAt",
        |v| int_in(v, 0, i64::MAX),
        Default::None,
    ),
    field("providerFamilyDomainMigrated", boolean, constant!(false)),
    field("nativeSearchEnhancementsEnabled", boolean, constant!(true)),
    Field {
        name: "onboardingOccupation",
        validate: |v| one_of(v, OCCUPATIONS),
        default: Default::None,
        nullable: true,
    },
    field("proactiveSuggestionsEnabled", boolean, Default::None),
    field("memoryEnabled", boolean, constant!(false)),
    field(
        "lastWorkspaceSession",
        workspace_session,
        Default::Value(|| Value::Array(Vec::new())),
    ),
    field(
        "lastActiveTabIndex",
        |v| int_in(v, 0, i64::MAX),
        constant!(0),
    ),
    field("lastActiveTaskByWorkspace", string_record, Default::None),
    field("dataBaseDir", trimmed_non_empty, Default::None),
    field(
        "pendingPostUpdateReleaseNotes",
        release_notes,
        Default::None,
    ),
    field("receivePreviewUpdates", boolean, constant!(false)),
    field("autoDownloadAndInstallUpdates", boolean, constant!(false)),
    field(
        "skippedElectronUpdateVersions",
        skipped_update_versions,
        Default::Value(|| Value::Object(Map::new())),
    ),
    field("settingsSyncFirstRunPromptHandled", boolean, Default::None),
    field("zcodeEndpointOrigin", endpoint_origin_value, Default::None),
];

// ---------------------------------------------------------------------------
// 预处理（按 appSettingsSchema 的嵌套顺序，由内到外执行）
// ---------------------------------------------------------------------------

fn strip_historical_resource_packages(target: &Value) -> Value {
    match target.as_object() {
        Some(source) if source.get("kind") == Some(&json!("ssh")) => {
            let mut next = source.clone();
            next.remove("resourcePackages");
            Value::Object(next)
        }
        _ => target.clone(),
    }
}

fn legacy_remote_history_entry(value: &Value) -> Option<(String, Map<String, Value>)> {
    let source = object(value)?;
    let mut sanitized = source.clone();
    if let Some(target) = source.get("target") {
        sanitized.insert("target".into(), strip_historical_resource_packages(target));
    }
    let id = trimmed_non_empty(sanitized.get("id")?)?;
    let mut entry = sanitized.clone();
    entry.insert("kind".into(), json!("remote"));
    let parsed = workspace_session_entry(&Value::Object(entry))?;
    Some((id.as_str()?.to_owned(), parsed.as_object()?.clone()))
}

fn migrate_legacy_workspace_session(raw: &mut Map<String, Value>) {
    let session: Vec<Value> = raw
        .get("lastWorkspaceSession")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let has_legacy_remote = session.iter().any(|entry| {
        entry
            .as_object()
            .is_some_and(|e| e.contains_key("historyId"))
    });
    let history: std::collections::HashMap<String, Map<String, Value>> = raw
        .get("remoteWorkspaceHistory")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(legacy_remote_history_entry)
                .collect()
        })
        .unwrap_or_default();

    let mut migrated: Vec<Value> = Vec::new();
    for entry in &session {
        let Some(source) = entry.as_object() else {
            continue;
        };
        let kind = source.get("kind").and_then(Value::as_str);
        if kind == Some("local")
            && let Some(path) = source.get("workspacePath").and_then(Value::as_str)
        {
            let purpose = if source.get("workspacePurpose") == Some(&json!("conversation")) {
                "conversation"
            } else {
                "project"
            };
            migrated
                .push(json!({"kind": "local", "workspacePath": path, "workspacePurpose": purpose}));
            continue;
        }
        if kind == Some("remote") {
            let target = source.get("target").filter(|target| truthy(target));
            if source.get("workspacePath").is_some_and(Value::is_string)
                && let Some(target) = target
            {
                let mut next = source.clone();
                next.insert("target".into(), strip_historical_resource_packages(target));
                migrated.push(Value::Object(next));
                continue;
            }
            if let Some(history_id) = source.get("historyId").and_then(Value::as_str)
                && let Some(legacy) = history.get(history_id)
            {
                let mut next = Map::new();
                next.insert("kind".into(), json!("remote"));
                for key in [
                    "workspacePath",
                    "localWorkspacePath",
                    "workspaceIdentity",
                    "target",
                    "lastOpenedAt",
                    "lastConnectionStatus",
                    "lastConnectionError",
                ] {
                    if let Some(value) = legacy.get(key) {
                        let keep = match key {
                            "localWorkspacePath" | "workspaceIdentity" | "lastConnectionError" => {
                                truthy(value)
                            }
                            _ => true,
                        };
                        if keep {
                            next.insert(key.to_owned(), value.clone());
                        }
                    }
                }
                migrated.push(Value::Object(next));
            }
        }
    }
    let legacy_tabs = raw.get("lastOpenTabs").and_then(Value::as_array).cloned();
    let existing_local: std::collections::HashSet<String> = migrated
        .iter()
        .filter(|entry| entry["kind"] == "local")
        .filter_map(|entry| entry["workspacePath"].as_str().map(str::to_owned))
        .collect();
    if let Some(tabs) = &legacy_tabs {
        for path in tabs.iter().filter_map(Value::as_str) {
            if !existing_local.contains(path) {
                migrated.push(
                    json!({"kind": "local", "workspacePath": path, "workspacePurpose": "project"}),
                );
            }
        }
    }
    if !migrated.is_empty() || has_legacy_remote || legacy_tabs.is_some() {
        raw.insert("lastWorkspaceSession".into(), Value::Array(migrated));
    }
    raw.remove("lastOpenTabs");
    raw.remove("remoteWorkspaceHistory");
}

/// JS 真值判断（用于迁移里的 `if (value)`）。
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn preprocess(raw: &mut Map<String, Value>) {
    migrate_legacy_workspace_session(raw);
    if let Some(origin) = raw.get("zcodeEndpointOrigin").cloned() {
        match origin.as_str().and_then(endpoint_origin) {
            Some(origin) => {
                raw.insert("zcodeEndpointOrigin".into(), Value::String(origin));
            }
            None => {
                raw.remove("zcodeEndpointOrigin");
            }
        }
    }
    if !raw.contains_key("localePreference")
        && let Some(locale) = raw.get("locale").and_then(|v| one_of(v, LOCALES))
    {
        raw.insert("localePreference".into(), locale);
    }
    if raw.get("closeToTrayOnWindowsMigrationInitialized") != Some(&Value::Bool(true)) {
        raw.insert("closeToTrayOnWindows".into(), Value::Bool(true));
        raw.insert(
            "closeToTrayOnWindowsMigrationInitialized".into(),
            Value::Bool(true),
        );
    }
    if raw.get("messageStreamShowReasoningMigrationInitialized") != Some(&Value::Bool(true)) {
        raw.insert("messageStreamShowReasoning".into(), Value::Bool(true));
        raw.insert(
            "messageStreamShowReasoningMigrationInitialized".into(),
            Value::Bool(true),
        );
    }
    if raw
        .get("desktopWindowSize")
        .is_some_and(|value| desktop_window_size(value).is_none())
    {
        raw.remove("desktopWindowSize");
    }
    if raw
        .get("embeddedBrowserViewportPreference")
        .is_some_and(|value| viewport_preference(value).is_none())
    {
        raw.remove("embeddedBrowserViewportPreference");
    }
}

fn validate(source: &Map<String, Value>) -> Option<Value> {
    let mut out = Map::new();
    for field in FIELDS {
        match source.get(field.name) {
            None => {
                if let Default::Value(default) = field.default {
                    out.insert(field.name.to_owned(), default());
                }
            }
            Some(Value::Null) if field.nullable => {
                out.insert(field.name.to_owned(), Value::Null);
            }
            Some(value) => {
                out.insert(field.name.to_owned(), (field.validate)(value)?);
            }
        }
    }
    Some(Value::Object(out))
}

/// `appSettingsSchema.safeParse`：成功返回规范化后的设置，失败返回 None（调用方回退默认值）。
pub fn parse_settings(raw: &Value) -> Option<Value> {
    let mut source = raw.as_object()?.clone();
    preprocess(&mut source);
    validate(&source)
}

/// `appSettingsSchema.parse({})`。
pub fn default_settings() -> Value {
    parse_settings(&json!({})).unwrap_or_else(|| json!({}))
}

// ---------------------------------------------------------------------------
// 旧账号连接迁移（读取边界）与回滚保留字段
// ---------------------------------------------------------------------------

fn record_of(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

pub fn needs_legacy_account_migration(raw: &Value) -> bool {
    let Some(raw) = raw.as_object() else {
        return false;
    };
    !raw.contains_key("providerFamilyConnectionSelections")
        && (raw.contains_key("modelProviderFamilySelectedKeys")
            || raw.contains_key("modelProviderFamilyModes"))
}

/// `decodeURIComponent`：格式错误的转义按 JS 抛错处理（返回 None）。
fn decode_uri_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn legacy_team_parts(key: &str, family: &str, expected: usize) -> Option<Vec<String>> {
    let prefix = format!("team-plan:builtin:{family}-coding-plan:");
    let rest = key.strip_prefix(&prefix)?;
    let parts = rest
        .split(':')
        .map(|part| decode_uri_component(part).map(|part| part.trim().to_owned()))
        .collect::<Option<Vec<_>>>()?;
    (parts.len() == expected && parts.iter().all(|part| !part.is_empty())).then_some(parts)
}

pub fn migrate_legacy_account_connections(raw: &Value) -> Value {
    if !needs_legacy_account_migration(raw) {
        return raw.clone();
    }
    let source = record_of(Some(raw));
    let modes = record_of(source.get("modelProviderFamilyModes"));
    let keys = record_of(source.get("modelProviderFamilySelectedKeys"));
    let mut selections = Map::new();
    for family in PROVIDER_FAMILIES {
        if modes.get(*family) == Some(&json!("apiKey")) {
            continue;
        }
        let key = keys
            .get(*family)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if key == format!("coding-plan:builtin:{family}-start-plan") {
            selections.insert((*family).to_owned(), json!({"kind": "start-plan"}));
        } else if key == format!("coding-plan:builtin:{family}-coding-plan") {
            selections.insert(
                (*family).to_owned(),
                json!({"kind": "individual-coding-plan"}),
            );
        } else if let Some(parts) = legacy_team_parts(key, family, 3) {
            selections.insert(
                (*family).to_owned(),
                json!({"kind": "team-coding-plan", "productId": parts[0], "organizationId": parts[1], "projectId": parts[2]}),
            );
        }
    }
    let mut next = source;
    next.insert(
        "providerFamilyConnectionSelections".into(),
        Value::Object(selections),
    );
    Value::Object(next)
}

/// 只有 2 段的旧 team 连接：等 Host 用 OAuth 补组织信息（此前普通写入不落连接选择）。
pub fn has_incomplete_legacy_team_connections(raw: &Value) -> bool {
    if !needs_legacy_account_migration(raw) {
        return false;
    }
    let source = record_of(Some(raw));
    let modes = record_of(source.get("modelProviderFamilyModes"));
    let keys = record_of(source.get("modelProviderFamilySelectedKeys"));
    PROVIDER_FAMILIES.iter().any(|family| {
        modes.get(*family) != Some(&json!("apiKey"))
            && keys
                .get(*family)
                .and_then(Value::as_str)
                .and_then(|key| legacy_team_parts(key.trim(), family, 2))
                .is_some()
    })
}

pub fn retained_legacy_fields(raw: &Value) -> Map<String, Value> {
    let source = record_of(Some(raw));
    [
        "modelProviderFamilyModes",
        "modelProviderFamilySelectedKeys",
    ]
    .into_iter()
    .filter_map(|key| source.get(key).map(|value| (key.to_owned(), value.clone())))
    .collect()
}

/// `shouldPersistSettingsMigrations`：读取时就需要把迁移结果写回。
pub fn should_persist_migrations(raw: &Value) -> bool {
    let Some(source) = raw.as_object() else {
        return false;
    };
    (needs_legacy_account_migration(raw) && !has_incomplete_legacy_team_connections(raw))
        || source.get("closeToTrayOnWindowsMigrationInitialized") != Some(&Value::Bool(true))
        || source.get("messageStreamShowReasoningMigrationInitialized") != Some(&Value::Bool(true))
}

// ---------------------------------------------------------------------------
// update(patch)：normalizeSettingsPatch + appSettingsPatchSchema
// ---------------------------------------------------------------------------

/// 规范化并校验后的 patch：`None` 表示删除该字段（TS 里是值为 undefined 的键）。
pub type Patch = Vec<(String, Option<Value>)>;

const TRIMMED_PATCH_FIELDS: &[&str] = &[
    "terminalFontFamily",
    "httpProxy",
    "httpProxyNoProxy",
    "httpProxyCaCertPath",
    "zcodeEndpointOrigin",
];

pub fn parse_patch(patch: &Value) -> Result<Patch, String> {
    let source = patch
        .as_object()
        .ok_or_else(|| "settings patch must be an object".to_owned())?;
    let mut normalized: Vec<(String, Option<Value>)> = source
        .iter()
        .map(|(key, value)| (key.clone(), Some(value.clone())))
        .collect();
    let has = |key: &str| source.contains_key(key);
    if has("locale")
        && !has("localePreference")
        && let Some(locale) = source.get("locale").filter(|v| v.is_string())
    {
        normalized.push(("localePreference".into(), Some(locale.clone())));
    }
    for (key, value) in normalized.iter_mut() {
        let Some(current) = value.clone() else {
            continue;
        };
        if TRIMMED_PATCH_FIELDS.contains(&key.as_str())
            && let Some(text) = current.as_str()
        {
            let trimmed = text.trim();
            *value = (!trimmed.is_empty()).then(|| Value::String(trimmed.to_owned()));
        } else if key == "providerFamilyDomain"
            && let Some(text) = current.as_str()
        {
            *value = (!text.trim().is_empty()).then(|| current.clone());
        } else if key == "integratedTerminalShell" {
            if current.get("mode") == Some(&json!("auto")) {
                *value = None;
            } else if current.get("mode") == Some(&json!("shell"))
                && let Some(object) = current.as_object()
            {
                let mut next = object.clone();
                for field in ["id", "label", "path"] {
                    if let Some(text) = object.get(field).and_then(Value::as_str) {
                        next.insert(field.to_owned(), Value::String(text.trim().to_owned()));
                    }
                }
                *value = Some(Value::Object(next));
            }
        }
    }

    let mut out = Vec::new();
    for (key, value) in normalized {
        let Some(field) = FIELDS.iter().find(|field| field.name == key) else {
            // z.object 默认丢弃未知键。
            continue;
        };
        let Some(value) = value else {
            out.push((key, None));
            continue;
        };
        let validated = match (&value, field.name) {
            (Value::Null, _) if field.nullable => Some(Value::Null),
            (Value::String(text), "providerFamilyDomain") if text.is_empty() => None,
            (_, "zcodeEndpointOrigin") => {
                // patch 里非法 origin 变成 undefined，即删除。
                out.push((
                    key,
                    value.as_str().and_then(endpoint_origin).map(Value::String),
                ));
                continue;
            }
            _ => Some((field.validate)(&value).ok_or_else(|| format!("invalid {key}"))?),
        };
        out.push((key, validated));
    }
    Ok(out)
}

/// `{...current, ...patch}` 后再整体 parse；删除的字段回到默认值或缺省。
pub fn merge(current: &Value, patch: &Patch) -> Option<Value> {
    let mut merged = current.as_object().cloned().unwrap_or_default();
    for (key, value) in patch {
        match value {
            Some(value) => {
                merged.insert(key.clone(), value.clone());
            }
            None => {
                merged.remove(key);
            }
        }
    }
    parse_settings(&Value::Object(merged))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_object_yields_documented_defaults_in_order() {
        let defaults = default_settings();
        let keys: Vec<&String> = defaults.as_object().unwrap().keys().collect();
        assert_eq!(keys.first().map(|k| k.as_str()), Some("recentProjects"));
        assert_eq!(defaults["locale"], "zh-CN");
        assert_eq!(defaults["closeToTrayOnWindowsMigrationInitialized"], true);
        assert_eq!(defaults["embeddedBrowserViewportPreference"]["zoom"], "fit");
        assert!(defaults.get("shortcutBindings").is_none());
        assert_eq!(keys.len(), 32);
    }

    #[test]
    fn legacy_file_is_migrated() {
        let raw = json!({
            "locale": "en-US",
            "closeToTrayOnWindows": false,
            "messageStreamShowReasoning": false,
            "unknownField": 1,
            "lastOpenTabs": ["/a", "/b"],
            "desktopWindowSize": {"width": 1, "height": 1, "maximized": false},
        });
        let parsed = parse_settings(&raw).unwrap();
        assert_eq!(parsed["localePreference"], "en-US");
        assert_eq!(parsed["closeToTrayOnWindows"], true);
        assert!(parsed.get("unknownField").is_none());
        assert!(parsed.get("desktopWindowSize").is_none());
        assert_eq!(
            parsed["lastWorkspaceSession"],
            json!([
                {"kind": "local", "workspacePath": "/a", "workspacePurpose": "project"},
                {"kind": "local", "workspacePath": "/b", "workspacePurpose": "project"},
            ])
        );
        assert!(should_persist_migrations(&raw));
    }

    #[test]
    fn invalid_field_fails_the_whole_file() {
        for raw in [
            json!({"httpProxy": "   "}),
            json!({"desktopZoomLevel": 9}),
            json!({"taskAutoArchiveOlderThanDays": 400}),
            json!({"onboardingOccupation": "pilot"}),
            json!({"skippedElectronUpdateVersions": {"beta": "1"}}),
            json!({"recentProjects": null}),
            json!([]),
        ] {
            assert!(parse_settings(&raw).is_none(), "{raw}");
        }
        assert_eq!(
            parse_settings(&json!({"onboardingOccupation": null})).unwrap()["onboardingOccupation"],
            Value::Null
        );
    }

    #[test]
    fn endpoint_origin_is_normalized_or_dropped() {
        let parsed =
            parse_settings(&json!({"zcodeEndpointOrigin": " https://x.example.com/path?q=1 "}))
                .unwrap();
        assert_eq!(parsed["zcodeEndpointOrigin"], "https://x.example.com");
        let dropped = parse_settings(&json!({"zcodeEndpointOrigin": "ftp://x"})).unwrap();
        assert!(dropped.get("zcodeEndpointOrigin").is_none());
    }

    #[test]
    fn patch_normalization_and_merge() {
        let current = default_settings();
        let patch = parse_patch(&json!({"locale": "en-US", "httpProxy": "  ", "terminalFontFamily": " Menlo ", "bogus": 1})).unwrap();
        let merged = merge(&current, &patch).unwrap();
        assert_eq!(merged["locale"], "en-US");
        assert_eq!(merged["localePreference"], "en-US");
        assert!(merged.get("httpProxy").is_none());
        assert_eq!(merged["terminalFontFamily"], "Menlo");
        assert!(parse_patch(&json!({"desktopZoomLevel": null})).is_err());
    }

    #[test]
    fn legacy_account_connections_migrate() {
        let raw = json!({
            "modelProviderFamilySelectedKeys": {
                "zai": "coding-plan:builtin:zai-start-plan",
                "bigmodel": "team-plan:builtin:bigmodel-coding-plan:p%201:o1:j1",
            },
        });
        let migrated = migrate_legacy_account_connections(&raw);
        assert_eq!(
            migrated["providerFamilyConnectionSelections"],
            json!({
                "zai": {"kind": "start-plan"},
                "bigmodel": {"kind": "team-coding-plan", "productId": "p 1", "organizationId": "o1", "projectId": "j1"},
            })
        );
        assert!(!has_incomplete_legacy_team_connections(&raw));
        let incomplete = json!({"modelProviderFamilySelectedKeys": {"zai": "team-plan:builtin:zai-coding-plan:p:j"}});
        assert!(has_incomplete_legacy_team_connections(&incomplete));
    }
}
