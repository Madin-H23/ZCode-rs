//! `hooks` channel（packages/services/src/hooks/hooksService.ts、workspaceHookSettingsModel.ts 与
//! packages/shared/src/workspace-hook-*.ts）。
//!
//! 项目 hooks 的声明 digest、bundle digest 与 TS 逐字节一致（CLI 用同一算法做 trust 判定）；trust
//! store 用 CLI domain 的同一 strict 解析器，损坏时整体 fail closed。预信任只转发给 workspace CLI。
use std::collections::HashSet;

use serde_json::{Map, Value, json};
use zcode_cli_domain::hooks::trust as trust_record;
use zcode_host_kit::{Reply, RpcError, ServiceBuilder, log};

use super::cli_config::{self, Update};
use super::skill_sync::truthy;
use super::{Roots, bind, fsx, json as js};
use crate::agent::{AgentRuntime, RuntimePolicy, WorkspaceTarget};

pub const CHANNEL: &str = "hooks";
const EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
];
const DEFAULT_TIMEOUT_MS: f64 = 60_000.0;
const DEFAULT_MAX_OUTPUT_BYTES: f64 = 32_768.0;

// ── schema（workspaceHooksConfigSchema） ──

fn positive(value: Option<&Value>) -> bool {
    value.is_none_or(|value| value.as_f64().is_some_and(|n| n.is_finite() && n > 0.0))
}

fn optional_bool(value: Option<&Value>) -> bool {
    value.is_none_or(Value::is_boolean)
}

fn non_empty_string(value: Option<&Value>) -> bool {
    value.is_some_and(|value| value.as_str().is_some_and(|text| !text.is_empty()))
}

fn optional_non_empty(value: Option<&Value>) -> bool {
    value.is_none() || non_empty_string(value)
}

fn valid_hook(hook: &Value) -> bool {
    let Some(hook) = hook.as_object() else {
        return false;
    };
    let common = non_empty_string(hook.get("command"))
        && optional_bool(hook.get("enabled"))
        && optional_non_empty(hook.get("statusMessage"))
        && positive(hook.get("timeoutMs"));
    match hook.get("type").and_then(Value::as_str) {
        Some("process") => {
            common
                && hook.get("args").is_none_or(|args| {
                    args.as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string))
                })
        }
        Some("command") => {
            common
                && optional_bool(hook.get("async"))
                && positive(hook.get("timeout"))
                && hook.get("shell").is_none_or(|shell| {
                    *shell == Value::Bool(true)
                        || shell.as_str().is_some_and(|text| !text.is_empty())
                })
        }
        _ => false,
    }
}

fn valid_matcher(matcher: &Value) -> bool {
    let Some(matcher) = matcher.as_object() else {
        return false;
    };
    matcher.keys().all(|key| key == "matcher" || key == "hooks")
        && optional_non_empty(matcher.get("matcher"))
        && matcher
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| !hooks.is_empty() && hooks.iter().all(valid_hook))
}

/// `workspaceHooksConfigSchema.safeParse(value).success`（undefined / null 都不合法）。
pub fn valid_hooks_config(value: Option<&Value>) -> bool {
    let Some(root) = value.and_then(Value::as_object) else {
        return false;
    };
    let keys_ok = root.keys().all(|key| {
        matches!(
            key.as_str(),
            "enabled" | "timeoutMs" | "maxOutputBytes" | "events"
        )
    });
    let events_ok = root.get("events").is_none_or(|events| {
        events.as_object().is_some_and(|events| {
            events.iter().all(|(name, matchers)| {
                EVENTS.contains(&name.as_str())
                    && matchers
                        .as_array()
                        .is_some_and(|matchers| matchers.iter().all(valid_matcher))
            })
        })
    });
    keys_ok
        && events_ok
        && optional_bool(root.get("enabled"))
        && positive(root.get("timeoutMs"))
        && positive(root.get("maxOutputBytes"))
}

// ── 来源与 digest（workspace-hook-config.ts / workspace-hook-digest.ts） ──

#[derive(Debug, Clone)]
struct Source {
    canonical_path: String,
    base_dir: String,
    discovery_order: usize,
    kind: &'static str,
    explicit: bool,
    editable: bool,
    hooks: Map<String, Value>,
}

fn source_input(
    path: &str,
    working_directory: &str,
    hooks: Map<String, Value>,
    order: usize,
    explicit: bool,
) -> Source {
    let canonical = fsx::resolve(path, &[]);
    let directory = fsx::dirname(&canonical);
    let base_dir = if fsx::basename(&directory) == ".zcode" {
        fsx::dirname(&directory)
    } else {
        directory
    };
    let kind = if explicit {
        "explicit"
    } else if fsx::basename(&canonical) == "zcode.json" {
        "zcode.json"
    } else {
        ".zcode/config.json"
    };
    Source {
        editable: !explicit
            && canonical == fsx::resolve(working_directory, &[".zcode", "config.json"]),
        canonical_path: canonical,
        base_dir,
        discovery_order: order,
        kind,
        explicit,
        hooks,
    }
}

#[derive(Debug, Clone, Copy)]
struct RuntimeRoot {
    enabled: bool,
    timeout_ms: f64,
    max_output_bytes: f64,
}

fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

fn runtime_root(roots: &[&Map<String, Value>]) -> RuntimeRoot {
    let mut enabled = false;
    let mut timeout = DEFAULT_TIMEOUT_MS;
    let mut max_output = DEFAULT_MAX_OUTPUT_BYTES;
    for root in roots {
        if root.get("enabled") == Some(&Value::Bool(true)) {
            enabled = true;
        }
        if let Some(value) = root.get("timeoutMs").and_then(Value::as_f64) {
            timeout = value;
        }
        if let Some(value) = root.get("maxOutputBytes").and_then(Value::as_f64) {
            max_output = value;
        }
    }
    RuntimeRoot {
        enabled,
        timeout_ms: js_round(timeout).max(1.0),
        max_output_bytes: js_round(max_output).max(1.0),
    }
}

/// `resolveWorkspaceHookTimeoutMs`。
fn hook_timeout_ms(hook: &Map<String, Value>, default: f64) -> f64 {
    let timeout = hook
        .get("timeoutMs")
        .and_then(Value::as_f64)
        .or_else(|| {
            (hook.get("type") == Some(&json!("command")))
                .then(|| {
                    hook.get("timeout")
                        .and_then(Value::as_f64)
                        .map(|t| t * 1000.0)
                })
                .flatten()
        })
        .unwrap_or(default);
    js_round(timeout).max(1.0)
}

fn number(value: f64) -> Value {
    js::number(value)
}

fn sha256(payload: &Value) -> String {
    fsx::sha256_hex(js::stringify(payload).as_bytes())
}

fn optional_payload(value: Option<&Value>) -> Value {
    match value {
        None => json!(["unset"]),
        Some(value) => json!(["set", value]),
    }
}

fn relative_source_path(workspace: &str, source: &str) -> String {
    let relative =
        fsx::relative(&fsx::resolve(workspace, &[]), &fsx::resolve(source, &[])).replace('\\', "/");
    if relative.is_empty() {
        fsx::basename(source)
    } else {
        relative
    }
}

#[derive(Debug, Clone)]
struct Slot<'a> {
    relative_path: &'a str,
    discovery_order: usize,
    event: &'a str,
    matcher: Value,
    matcher_index: usize,
    hook_index: usize,
}

fn declaration_digest(
    hook: &Map<String, Value>,
    slot: &Slot,
    default_timeout: f64,
    max_output: f64,
) -> String {
    let execution = if hook.get("type") == Some(&json!("process")) {
        json!([
            "process",
            hook.get("command"),
            hook.get("args").cloned().unwrap_or(json!([]))
        ])
    } else {
        let shell = match hook.get("shell") {
            None => json!(["unset"]),
            Some(Value::Bool(true)) => json!(["true"]),
            Some(other) => json!(["string", other]),
        };
        json!([
            "command",
            hook.get("command"),
            hook.get("async") == Some(&Value::Bool(true)),
            shell
        ])
    };
    sha256(&json!([
        "workspace-hook-declaration",
        1,
        slot.relative_path,
        slot.discovery_order,
        slot.event,
        slot.matcher,
        slot.matcher_index,
        slot.hook_index,
        execution,
        number(hook_timeout_ms(hook, default_timeout)),
        number(max_output),
    ]))
}

/// `CanonicalWorkspaceHookEntryData`（键序同 TS）。
#[derive(Debug, Clone)]
struct HookEntry {
    value: Map<String, Value>,
    event: &'static str,
    matcher_index: usize,
    hook_index: usize,
    source_index: usize,
}

fn entries(workspace: &str, sources: &[Source], root: RuntimeRoot) -> Vec<HookEntry> {
    let mut result = Vec::new();
    for (source_index, source) in sources.iter().enumerate() {
        let relative = relative_source_path(workspace, &source.canonical_path);
        let source_enabled = source.hooks.get("enabled") != Some(&Value::Bool(false));
        let events = js::record(source.hooks.get("events"));
        for event in EVENTS {
            let matchers = events
                .get(event)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (matcher_index, matcher) in matchers.iter().enumerate() {
                let matcher_value = matcher.get("matcher").cloned().unwrap_or(Value::Null);
                let hooks = matcher
                    .get("hooks")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (hook_index, hook) in hooks.iter().enumerate() {
                    let Some(hook) = hook.as_object() else {
                        continue;
                    };
                    let declaration_enabled = hook.get("enabled") != Some(&Value::Bool(false));
                    let configured = source_enabled && declaration_enabled && root.enabled;
                    let slot = Slot {
                        relative_path: &relative,
                        discovery_order: source.discovery_order,
                        event,
                        matcher: matcher_value.clone(),
                        matcher_index,
                        hook_index,
                    };
                    let digest =
                        declaration_digest(hook, &slot, root.timeout_ms, root.max_output_bytes);
                    let mut value = Map::new();
                    value.insert(
                        "reviewItemId".into(),
                        json!(format!(
                            "workspace-hook-{source_index}-{event}-{matcher_index}-{hook_index}"
                        )),
                    );
                    value.insert("event".into(), json!(event));
                    value.insert("matcherIndex".into(), json!(matcher_index));
                    value.insert("hookIndex".into(), json!(hook_index));
                    value.insert("sourceFileIndex".into(), json!(source_index));
                    value.insert("sourceRelativePath".into(), json!(relative));
                    value.insert("matcher".into(), matcher_value.clone());
                    value.insert(
                        "command".into(),
                        hook.get("command").cloned().unwrap_or(Value::Null),
                    );
                    value.insert(
                        "resolvedTimeoutMs".into(),
                        number(hook_timeout_ms(hook, root.timeout_ms)),
                    );
                    value.insert(
                        "resolvedMaxOutputBytes".into(),
                        number(root.max_output_bytes),
                    );
                    if let Some(message) = hook.get("statusMessage").filter(|value| truthy(value)) {
                        value.insert("statusMessage".into(), message.clone());
                    }
                    value.insert("sourceRootEnabled".into(), json!(source_enabled));
                    value.insert("declarationEnabled".into(), json!(declaration_enabled));
                    value.insert("runtimeHooksEnabled".into(), json!(root.enabled));
                    value.insert("configuredEnabled".into(), json!(configured));
                    value.insert("editable".into(), json!(source.editable));
                    value.insert("declarationDigestAlgorithm".into(), json!("sha256"));
                    value.insert("hookDeclarationDigest".into(), json!(digest));
                    if hook.get("type") == Some(&json!("process")) {
                        value.insert("type".into(), json!("process"));
                        if let Some(args) = hook
                            .get("args")
                            .and_then(Value::as_array)
                            .filter(|args| !args.is_empty())
                        {
                            value.insert("args".into(), Value::Array(args.clone()));
                        }
                    } else {
                        value.insert("type".into(), json!("command"));
                        if hook.get("async") == Some(&Value::Bool(true)) {
                            value.insert("async".into(), json!(true));
                        }
                        if let Some(shell) = hook.get("shell") {
                            value.insert("shell".into(), shell.clone());
                        }
                    }
                    result.push(HookEntry {
                        value,
                        event,
                        matcher_index,
                        hook_index,
                        source_index,
                    });
                }
            }
        }
    }
    result
}

fn digest_of(entry: &HookEntry) -> String {
    entry.value["hookDeclarationDigest"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

/// `buildWorkspaceHookBundleSnapshot`：没有任何 hook 时为 `None`。
fn snapshot(
    identity: &str,
    workspace: &str,
    sources: &[Source],
    entries: &[HookEntry],
) -> Option<Value> {
    if entries.is_empty() {
        return None;
    }
    let source_files: Vec<Value> = sources
        .iter()
        .map(|source| {
            let mut root = Map::new();
            for key in ["enabled", "timeoutMs", "maxOutputBytes"] {
                if let Some(value) = source.hooks.get(key) {
                    root.insert(key.into(), value.clone());
                }
            }
            json!({
                "canonicalPath": source.canonical_path,
                "baseDir": source.base_dir,
                "discoveryOrder": source.discovery_order,
                "configFileKind": source.kind,
                "explicitProjectConfig": source.explicit,
                "editable": source.editable,
                "hooksRoot": root,
            })
        })
        .collect();
    let payload = json!([
        "workspace-hook-bundle",
        1,
        sources
            .iter()
            .map(|source| json!([
                relative_source_path(workspace, &source.canonical_path),
                source.discovery_order,
                source.kind,
                source.explicit,
                optional_payload(source.hooks.get("enabled")),
                optional_payload(source.hooks.get("timeoutMs")),
                optional_payload(source.hooks.get("maxOutputBytes")),
            ]))
            .collect::<Vec<_>>(),
        entries
            .iter()
            .map(|entry| json!([
                entry.value["hookDeclarationDigest"],
                entry.value["sourceRootEnabled"],
                entry.value["declarationEnabled"],
                entry.value["runtimeHooksEnabled"],
                entry.value["configuredEnabled"],
            ]))
            .collect::<Vec<_>>(),
    ]);
    Some(json!({
        "schemaVersion": 1,
        "workspaceIdentity": identity,
        "discoveredAt": iso_now(),
        "sourceFiles": source_files,
        "hooks": entries.iter().map(|entry| Value::Object(entry.value.clone())).collect::<Vec<_>>(),
        "digestAlgorithm": "sha256",
        "bundleDigest": sha256(&payload),
    }))
}

fn iso_now() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    zcode_cli_domain::hooks::input::iso_timestamp(ms)
}

// ── Hook 投影（workspaceHookSettingsModel.ts） ──

const KNOWN_FIELDS: [&str; 9] = [
    "type",
    "command",
    "args",
    "async",
    "enabled",
    "shell",
    "statusMessage",
    "timeout",
    "timeoutMs",
];

fn custom_fields(hook: &Map<String, Value>) -> Option<Value> {
    let custom: Map<String, Value> = hook
        .iter()
        .filter(|(key, _)| !KNOWN_FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    (!custom.is_empty()).then_some(Value::Object(custom))
}

fn configured_state(entry: &HookEntry, source_path: &str) -> Map<String, Value> {
    let mut state = Map::new();
    for key in [
        "sourceRootEnabled",
        "declarationEnabled",
        "runtimeHooksEnabled",
        "configuredEnabled",
    ] {
        state.insert(key.into(), entry.value[key].clone());
    }
    state.insert("sourcePath".into(), json!(source_path));
    state
}

fn raw_hook<'a>(source: &'a Source, entry: &HookEntry) -> Option<&'a Map<String, Value>> {
    source
        .hooks
        .get("events")?
        .get(entry.event)?
        .get(entry.matcher_index)?
        .get("hooks")?
        .get(entry.hook_index)?
        .as_object()
}

fn to_hook(
    entry: &HookEntry,
    source: &Source,
    id: String,
    location: Value,
    workspace_hook: Option<Map<String, Value>>,
) -> Value {
    let raw = raw_hook(source, entry).cloned().unwrap_or_default();
    let state = configured_state(entry, &source.canonical_path);
    let mut hook = Map::new();
    hook.insert("id".into(), json!(id));
    hook.insert("event".into(), json!(entry.event));
    if let Some(matcher) = entry.value.get("matcher").filter(|value| !value.is_null()) {
        hook.insert("matcher".into(), matcher.clone());
    }
    let kind = entry.value["type"].clone();
    hook.insert("type".into(), kind.clone());
    hook.insert("command".into(), entry.value["command"].clone());
    if kind == "process" {
        hook.insert(
            "args".into(),
            entry.value.get("args").cloned().unwrap_or(json!([])),
        );
    }
    if kind == "command" {
        if let Some(value) = entry.value.get("async") {
            hook.insert("async".into(), value.clone());
        }
        if let Some(value) = entry.value.get("shell") {
            hook.insert("shell".into(), value.clone());
        }
    }
    if let Some(message) = entry.value.get("statusMessage") {
        hook.insert("statusMessage".into(), message.clone());
    }
    let timeout = raw
        .get("timeout")
        .filter(|_| raw.get("type") == Some(&json!("command")))
        .cloned()
        .or_else(|| {
            raw.get("timeoutMs")
                .and_then(Value::as_f64)
                .map(|ms| number(js_round(ms) / 1000.0))
        });
    if let Some(timeout) = timeout {
        hook.insert("timeout".into(), timeout);
    }
    hook.insert("enabled".into(), entry.value["configuredEnabled"].clone());
    hook.insert("editable".into(), entry.value["editable"].clone());
    hook.insert("configuredState".into(), Value::Object(state.clone()));
    if let Some(extra) = workspace_hook {
        let mut workspace = state;
        workspace.insert("reviewItemId".into(), entry.value["reviewItemId"].clone());
        workspace.insert("sourceFileIndex".into(), json!(entry.source_index));
        for (key, value) in extra {
            workspace.insert(key, value);
        }
        hook.insert("workspaceHook".into(), Value::Object(workspace));
    }
    if let Some(custom) = custom_fields(&raw) {
        hook.insert("custom".into(), custom);
    }
    hook.insert("location".into(), location);
    Value::Object(hook)
}

fn legacy_hooks(config: Option<&Map<String, Value>>, location: &Value) -> Vec<Value> {
    let mut hooks = Vec::new();
    let mut counter = 0usize;
    let source = location["source"].as_str().unwrap_or("");
    let scope = location["scope"].as_str().unwrap_or("");
    let events = config
        .map(|config| js::record(config.get("hooks")))
        .unwrap_or_default();
    for (event, matchers) in &events {
        if !EVENTS.contains(&event.as_str()) {
            continue;
        }
        let Some(matchers) = matchers.as_array() else {
            continue;
        };
        for matcher in matchers {
            for hook in matcher
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let kind = hook.get("type").and_then(Value::as_str);
                if !matches!(kind, Some("command" | "process"))
                    || !hook.get("command").is_some_and(truthy)
                {
                    continue;
                }
                let mut value = Map::new();
                value.insert(
                    "id".into(),
                    json!(format!("hook-{source}-{scope}-{counter}")),
                );
                counter += 1;
                value.insert("event".into(), json!(event));
                if let Some(matcher) = matcher.get("matcher") {
                    value.insert("matcher".into(), matcher.clone());
                }
                value.insert("type".into(), json!(kind));
                value.insert("command".into(), hook["command"].clone());
                if kind == Some("process") {
                    value.insert(
                        "args".into(),
                        hook.get("args")
                            .filter(|v| !v.is_null())
                            .cloned()
                            .unwrap_or(json!([])),
                    );
                }
                for key in ["async", "shell"] {
                    if let Some(item) = hook.get(key) {
                        value.insert(key.into(), item.clone());
                    }
                }
                if let Some(message) = hook.get("statusMessage").filter(|value| truthy(value)) {
                    value.insert("statusMessage".into(), message.clone());
                }
                let timeout = hook
                    .get("timeout")
                    .filter(|value| !value.is_null())
                    .cloned()
                    .or_else(|| {
                        hook.get("timeoutMs")
                            .and_then(Value::as_f64)
                            .map(|ms| number(js_round(ms / 1000.0)))
                    });
                if let Some(timeout) = timeout {
                    value.insert("timeout".into(), timeout);
                }
                value.insert("enabled".into(), json!(false));
                value.insert("editable".into(), json!(false));
                value.insert("location".into(), location.clone());
                hooks.push(Value::Object(value));
            }
        }
    }
    hooks
}

/// `toZCodeHooksEvents`：按事件、matcher 聚合（matcher 用严格相等比较）。
fn to_events(hooks: &[Value]) -> Value {
    let mut events: Map<String, Value> = Map::new();
    let mut matcher_keys: Vec<(String, Vec<Option<Value>>)> = Vec::new();
    for hook in hooks {
        let event = hook
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let matcher = hook
            .get("matcher")
            .filter(|value| !value.is_null())
            .cloned();
        let index = matcher_keys.iter().position(|(name, _)| *name == event);
        let (keys_index, position) = match index {
            Some(index) => {
                let position = matcher_keys[index]
                    .1
                    .iter()
                    .position(|key| strict_equal(key.as_ref(), matcher.as_ref()));
                (index, position)
            }
            None => {
                matcher_keys.push((event.clone(), Vec::new()));
                (matcher_keys.len() - 1, None)
            }
        };
        let list = events.entry(event.clone()).or_insert_with(|| json!([]));
        let list = list.as_array_mut().expect("events list");
        let position = match position {
            Some(position) => position,
            None => {
                let mut group = Map::new();
                let stored = matcher.clone().filter(truthy);
                if let Some(value) = &stored {
                    group.insert("matcher".into(), value.clone());
                }
                group.insert("hooks".into(), json!([]));
                list.push(Value::Object(group));
                matcher_keys[keys_index].1.push(stored);
                list.len() - 1
            }
        };
        if let Some(group_hooks) = list[position]
            .get_mut("hooks")
            .and_then(Value::as_array_mut)
        {
            group_hooks.push(writable_hook(hook));
        }
    }
    Value::Object(events)
}

/// JS `===` 在 matcher 上的语义（只会是字符串或缺省）。
fn strict_equal(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(Value::String(a)), Some(Value::String(b))) => a == b,
        (Some(Value::Number(a)), Some(Value::Number(b))) => a.as_f64() == b.as_f64(),
        (Some(Value::Bool(a)), Some(Value::Bool(b))) => a == b,
        _ => false,
    }
}

/// `getWritableHook`。
fn writable_hook(hook: &Value) -> Value {
    let mut out = js::record(hook.get("custom"));
    out.insert(
        "type".into(),
        hook.get("type").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "command".into(),
        hook.get("command").cloned().unwrap_or(Value::Null),
    );
    let enabled = hook.get("enabled").cloned().unwrap_or(Value::Null);
    let declaration = match hook.get("configuredState").and_then(Value::as_object) {
        None => enabled.clone(),
        Some(state) if state.get("configuredEnabled") == Some(&enabled) => state
            .get("declarationEnabled")
            .cloned()
            .unwrap_or(Value::Null),
        Some(_) => enabled.clone(),
    };
    out.insert("enabled".into(), declaration);
    if let Some(message) = hook.get("statusMessage").filter(|value| truthy(value)) {
        out.insert("statusMessage".into(), message.clone());
    }
    let timeout = hook.get("timeout").filter(|value| truthy(value));
    if hook.get("type") == Some(&json!("command")) {
        if hook.get("async").is_some_and(truthy) {
            out.insert("async".into(), json!(true));
        }
        if let Some(shell) = hook.get("shell").filter(|value| truthy(value)) {
            out.insert("shell".into(), shell.clone());
        }
        if let Some(timeout) = timeout {
            out.insert("timeout".into(), timeout.clone());
        }
    } else {
        if let Some(args) = hook
            .get("args")
            .and_then(Value::as_array)
            .filter(|args| !args.is_empty())
        {
            out.insert("args".into(), Value::Array(args.clone()));
        }
        if let Some(timeout) = timeout.and_then(Value::as_f64) {
            out.insert("timeoutMs".into(), number(timeout * 1000.0));
        }
    }
    Value::Object(out)
}

/// `resolveNextRootEnabled`。
fn next_root_enabled(existing: Option<&Value>, hooks: &[Value]) -> Option<Value> {
    let enables = hooks.iter().any(|hook| {
        let enabled = hook.get("enabled").is_some_and(truthy);
        enabled
            && hook
                .get("configuredState")
                .and_then(Value::as_object)
                .is_none_or(|state| state.get("configuredEnabled") != hook.get("enabled"))
    });
    if enables {
        Some(json!(true))
    } else {
        existing.cloned()
    }
}

// ── 服务 ──

#[derive(Clone)]
pub struct HooksService {
    roots: Roots,
    runtime: Option<AgentRuntime>,
}

impl HooksService {
    pub fn new(roots: Roots, runtime: Option<AgentRuntime>) -> Self {
        Self { roots, runtime }
    }

    fn root_dir(&self, source: &str, workspace: Option<&str>) -> String {
        let base = workspace
            .map(str::to_owned)
            .unwrap_or_else(|| self.roots.home.clone());
        match (source, workspace) {
            ("zcode", Some(_)) => fsx::join(&base, &[".zcode"]),
            ("zcode", None) => fsx::join(&base, &[".zcode", "cli"]),
            ("agents", _) => fsx::join(&base, &[".agents"]),
            _ => fsx::join(&base, &[".claude"]),
        }
    }

    fn config_path(&self, source: &str, workspace: Option<&str>) -> String {
        let file = if source == "zcode" {
            "config.json"
        } else {
            "settings.json"
        };
        fsx::join(&self.root_dir(source, workspace), &[file])
    }

    fn location(&self, source: &str, workspace: Option<&str>) -> Value {
        let mut location = Map::new();
        location.insert("source".into(), json!(source));
        location.insert(
            "scope".into(),
            json!(if workspace.is_some() {
                "project"
            } else {
                "user"
            }),
        );
        location.insert(
            "directoryPath".into(),
            json!(self.root_dir(source, workspace)),
        );
        if let Some(workspace) = workspace {
            location.insert("projectPath".into(), json!(workspace));
        }
        Value::Object(location)
    }

    async fn read_json(path: &str) -> Option<Map<String, Value>> {
        js::parse_record(&fsx::read_text_opt(path).await?)
    }

    /// 项目配置目录：自 workspace 向上到 worktree 根（含 `.git` 目录或文件），自顶向下。
    async fn project_directories(start: &str) -> Vec<String> {
        let mut directories = Vec::new();
        let mut current = start.to_owned();
        loop {
            directories.push(current.clone());
            let marker = tokio::fs::metadata(fsx::join(&current, &[".git"])).await;
            if marker.is_ok_and(|meta| meta.is_dir() || meta.is_file()) {
                directories.reverse();
                return directories;
            }
            let parent = fsx::dirname(&current);
            if parent == current {
                break;
            }
            current = parent;
        }
        vec![start.to_owned()]
    }

    async fn project_sources(working_directory: &str) -> Vec<Source> {
        let start = fsx::resolve(working_directory, &[]);
        let mut refs: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for directory in Self::project_directories(&start).await {
            for candidate in [
                fsx::join(&directory, &["zcode.json"]),
                fsx::join(&directory, &[".zcode", "config.json"]),
            ] {
                if fsx::exists(&candidate).await && seen.insert(fsx::resolve(&candidate, &[])) {
                    refs.push(candidate);
                }
            }
        }
        let mut sources = Vec::new();
        for (order, path) in refs.iter().enumerate() {
            let Some(raw) = fsx::read_text_opt(path).await else {
                continue;
            };
            let Ok(Value::Object(value)) = js::parse(&raw) else {
                continue;
            };
            let Some(hooks) = value.get("hooks") else {
                continue;
            };
            if !valid_hooks_config(Some(hooks)) {
                continue;
            }
            sources.push(source_input(
                path,
                working_directory,
                hooks.as_object().cloned().unwrap_or_default(),
                order,
                false,
            ));
        }
        sources
    }

    async fn user_source(&self) -> Option<Source> {
        let path = self.config_path("zcode", None);
        let config = Self::read_json(&path).await;
        let hooks = config.as_ref().and_then(|config| config.get("hooks"));
        if !valid_hooks_config(hooks) {
            return None;
        }
        let mut source = source_input(
            &path,
            &self.roots.home,
            hooks?.as_object()?.clone(),
            0,
            true,
        );
        source.editable = true;
        Some(source)
    }

    /// trust store 中本 workspace 的 digest；读不了或结构不合规整体视为损坏（fail closed）。
    async fn trusted_digests(&self, identity: &str) -> (HashSet<String>, bool) {
        let config = Self::read_json(&self.config_path("zcode", None))
            .await
            .unwrap_or_default();
        let storage = js::record(config.get("storage"));
        let configured = storage
            .get("dir")
            .and_then(Value::as_str)
            .map(|dir| zcode_cli_domain::js_string::trim(dir).to_owned())
            .unwrap_or_default();
        let home = &self.roots.home;
        let root = if configured.is_empty() {
            fsx::join(home, &[".zcode"])
        } else if let Some(rest) = configured.strip_prefix("~/") {
            fsx::join(home, &[rest])
        } else if fsx::is_absolute(&configured) {
            fsx::resolve(&configured, &[])
        } else {
            fsx::resolve(home, &[&configured])
        };
        let path = fsx::join(&root, &["security", "workspace-hook-trust-v1.json"]);
        let content = match fsx::read_text(&path).await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (HashSet::new(), false);
            }
            Err(error) => {
                log::warn(format!(
                    "Workspace Hook Trust store 不可读，已 fail-closed 忽略全部持久信任记录 path={path} error={}",
                    fsx::fs_message(&error, "open", &path)
                ));
                return (HashSet::new(), true);
            }
        };
        match trust_record::parse_store(&content) {
            Some(records) => (
                records
                    .into_iter()
                    .filter(|record| record.workspace_identity == identity)
                    .map(|record| record.hook_declaration_digest)
                    .collect(),
                false,
            ),
            None => {
                log::warn(format!(
                    "Workspace Hook Trust store 结构不符合 schema，已 fail-closed 忽略全部持久信任记录 path={path}"
                ));
                (HashSet::new(), true)
            }
        }
    }

    pub async fn load(&self, params: Value) -> Result<Value, RpcError> {
        let raw_path = params
            .get("workspacePath")
            .and_then(Value::as_str)
            .unwrap_or("");
        let workspace = fsx::resolve(raw_path, &[]);
        let identity = params
            .get("workspaceIdentity")
            .and_then(Value::as_str)
            .map(|identity| zcode_cli_domain::js_string::trim(identity).to_owned())
            .filter(|identity| !identity.is_empty())
            .unwrap_or_else(|| workspace.clone());
        let project = Self::project_sources(&workspace).await;
        let user = self.user_source().await;
        let mut roots: Vec<&Map<String, Value>> = Vec::new();
        if let Some(user) = &user {
            roots.push(&user.hooks);
        }
        roots.extend(project.iter().map(|source| &source.hooks));
        let root = runtime_root(&roots);
        let project_entries = entries(&workspace, &project, root);
        let snapshot = snapshot(&identity, &workspace, &project, &project_entries);
        let (trusted, corrupt) = self.trusted_digests(&identity).await;
        let mut hooks = Vec::new();
        if let Some(snapshot) = &snapshot {
            let bundle = snapshot["bundleDigest"].clone();
            for entry in &project_entries {
                let source = &project[entry.source_index];
                let digest = digest_of(entry);
                let mut extra = Map::new();
                extra.insert("workspaceIdentity".into(), json!(identity));
                extra.insert("bundleDigest".into(), bundle.clone());
                extra.insert("hookDeclarationDigest".into(), json!(digest));
                extra.insert(
                    "trustState".into(),
                    json!(if trusted.contains(&digest) {
                        "trusted_persistent"
                    } else {
                        "pending_trust"
                    }),
                );
                let location = json!({
                    "source": "zcode",
                    "scope": "project",
                    "directoryPath": fsx::dirname(&source.canonical_path),
                    "projectPath": workspace,
                });
                hooks.push(to_hook(
                    entry,
                    source,
                    entry.value["reviewItemId"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                    location,
                    Some(extra),
                ));
            }
        }
        for source in ["agents", "claude"] {
            let config = Self::read_json(&self.config_path(source, Some(&workspace))).await;
            hooks.extend(legacy_hooks(
                config.as_ref(),
                &self.location(source, Some(&workspace)),
            ));
        }
        if let Some(user) = &user {
            let user_entries = entries(&workspace, std::slice::from_ref(user), root);
            let location = self.location("zcode", None);
            for (index, entry) in user_entries.iter().enumerate() {
                hooks.push(to_hook(
                    entry,
                    user,
                    format!("hook-zcode-user-{index}"),
                    location.clone(),
                    None,
                ));
            }
        }
        for source in ["agents", "claude"] {
            let config = Self::read_json(&self.config_path(source, None)).await;
            hooks.extend(legacy_hooks(config.as_ref(), &self.location(source, None)));
        }
        let enabled = hooks
            .iter()
            .any(|hook| hook.get("enabled").is_some_and(truthy));
        let mut result = Map::new();
        result.insert("hooks".into(), Value::Array(hooks));
        result.insert("hooksEnabled".into(), json!(enabled));
        if let Some(snapshot) = snapshot {
            result.insert("workspaceHookSnapshot".into(), snapshot);
        }
        if corrupt {
            result.insert("trustStoreCorrupt".into(), json!(true));
        }
        Ok(Value::Object(result))
    }

    async fn write_config(&self, workspace: Option<&str>, hooks: &[Value]) -> Result<(), RpcError> {
        let path = self.config_path("zcode", workspace);
        cli_config::update(&path, |raw| {
            let mut existing = raw
                .as_deref()
                .and_then(js::parse_record)
                .unwrap_or_default();
            let mut root = js::record(existing.get("hooks"));
            if let Some(enabled) = next_root_enabled(root.get("enabled"), hooks) {
                root.insert("enabled".into(), enabled);
            }
            root.insert("events".into(), to_events(hooks));
            existing.insert("hooks".into(), Value::Object(root));
            Ok(Update::Write(js::pretty_line(&Value::Object(existing)), ()))
        })
        .await
    }

    pub async fn save(&self, params: Value) -> Result<(), RpcError> {
        let workspace = params
            .get("workspacePath")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let current = fsx::resolve(&workspace, &[".zcode", "config.json"]);
        let hooks = params
            .get("hooks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let editable = |hook: &&Value| hook.get("editable") != Some(&Value::Bool(false));
        let user: Vec<Value> = hooks
            .iter()
            .filter(editable)
            .filter(
                |hook| match hook.get("location").filter(|location| !location.is_null()) {
                    None => true,
                    Some(location) => location["source"] == "zcode" && location["scope"] == "user",
                },
            )
            .cloned()
            .collect();
        let project: Vec<Value> = hooks
            .iter()
            .filter(editable)
            .filter(|hook| {
                let location = &hook["location"];
                location["source"] == "zcode"
                    && location["scope"] == "project"
                    && hook
                        .get("configuredState")
                        .filter(|state| truthy(state))
                        .is_none_or(|state| {
                            fsx::resolve(state["sourcePath"].as_str().unwrap_or(""), &[]) == current
                        })
            })
            .cloned()
            .collect();
        self.write_config(None, &user).await?;
        self.write_config(Some(&workspace), &project).await
    }

    pub async fn grant_trust(&self, params: Value) -> Result<Value, RpcError> {
        let runtime = self.runtime.clone().ok_or_else(|| {
            fsx::error("Workspace Hook trust grant is unavailable in this runtime")
        })?;
        let target = WorkspaceTarget::from_value(&params)?;
        // 没有 task 也允许预信任：只拉起该 workspace 的 CLI 控制面，不伪造 session。
        let process = runtime
            .get_client(&target, RuntimePolicy::StartIfNeeded)
            .await?;
        process
            .request_value(
                "workspace/hooks/trustGrant",
                &json!({
                    "workspace": target.workspace_ref(),
                    "bundleDigest": params.get("bundleDigest"),
                    "hookDeclarationDigest": params.get("hookDeclarationDigest"),
                }),
            )
            .await
    }
}

pub fn service(hooks: HooksService) -> ServiceBuilder {
    let builder = ServiceBuilder::new(CHANNEL);
    let builder = bind(builder, "loadHooks", &hooks, |s, p| async move {
        s.load(p).await.map(Reply::Value)
    });
    let builder = bind(builder, "saveHooks", &hooks, |s, p| async move {
        s.save(p).await.map(|()| Reply::Undefined)
    });
    bind(
        builder,
        "grantWorkspaceHookTrust",
        &hooks,
        |s, p| async move { s.grant_trust(p).await.map(Reply::Value) },
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn schema_matches_zod() {
        assert!(valid_hooks_config(Some(
            &json!({"enabled":true,"events":{"Stop":[{"hooks":[{"type":"command","command":"x","shell":true,"extra":1}]}]}})
        )));
        assert!(!valid_hooks_config(Some(&json!({"events":{"Nope":[]}}))));
        assert!(!valid_hooks_config(Some(
            &json!({"events":{"Stop":[{"hooks":[]}]}})
        )));
        assert!(!valid_hooks_config(Some(
            &json!({"events":{"Stop":[{"hooks":[{"type":"command","command":"x","shell":false}]}]}})
        )));
        assert!(!valid_hooks_config(Some(&json!({"other":1}))));
        assert!(!valid_hooks_config(None));
        assert!(!valid_hooks_config(Some(&Value::Null)));
    }

    #[tokio::test]
    async fn load_and_save_round_trip() {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("repo");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(workspace.join(".zcode")).unwrap();
        std::fs::write(
            workspace.join(".zcode/config.json"),
            r#"{"keep":1,"hooks":{"enabled":true,"events":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi","timeout":5,"note":"x"}]}]}}}"#,
        )
        .unwrap();
        let service = HooksService::new(Roots::new(home.path(), home.path()), None);
        let loaded = service
            .load(json!({"workspacePath": workspace.display().to_string()}))
            .await
            .unwrap();
        let hook = &loaded["hooks"][0];
        assert_eq!(hook["id"], "workspace-hook-0-PreToolUse-0-0");
        assert_eq!(hook["timeout"], 5);
        assert_eq!(hook["custom"], json!({"note":"x"}));
        assert_eq!(hook["workspaceHook"]["trustState"], "pending_trust");
        assert_eq!(loaded["hooksEnabled"], true);
        assert_eq!(
            loaded["workspaceHookSnapshot"]["hooks"][0]["resolvedTimeoutMs"],
            5000
        );
        service
            .save(
                json!({"workspacePath": workspace.display().to_string(), "hooks": loaded["hooks"]}),
            )
            .await
            .unwrap();
        let text = std::fs::read_to_string(workspace.join(".zcode/config.json")).unwrap();
        let saved: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(saved["keep"], 1);
        assert_eq!(
            saved["hooks"]["events"]["PreToolUse"][0],
            json!({"matcher":"Bash","hooks":[{"note":"x","type":"command","command":"echo hi","enabled":true,"timeout":5}]})
        );
        let user = std::fs::read_to_string(home.path().join(".zcode/cli/config.json")).unwrap();
        assert_eq!(user, "{\n  \"hooks\": {\n    \"events\": {}\n  }\n}\n");
    }
}
