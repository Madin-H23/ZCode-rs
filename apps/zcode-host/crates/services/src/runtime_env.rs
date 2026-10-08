//! 子进程的基础环境（packages/shared/src/runtimeEnv.ts、services/src/runtime-tools/runtimeCommandEnv.ts）。
//!
//! Node Host 会就地清洗自身 `process.env` 再叠加 Main 采集的登录 shell 补丁；Rust Host 不改写
//! 自身环境，而是在这里算出同样的“有效基础环境”，由 CLI、git、终端等子进程显式使用。
use std::collections::BTreeMap;

const SANITIZED_KEYS: &[&str] = &[
    "NODE_ENV",
    "ELECTRON_RUN_AS_NODE",
    "NODE_NO_WARNINGS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "GIT_SSL_CAINFO",
    "ZCODE_REMOTE_RUNTIME_NETWORK_AUTHORITY",
    "ZCODE_REMOTE_HTTP_PROXY",
    "ZCODE_REMOTE_NO_PROXY",
    "ZCODE_CUA_PERMISSION_BROKER_SOCKET",
    "ZCODE_CUA_PERMISSION_BROKER_TOKEN",
    "ZCODE_CUA_PERMISSION_BROKER_REFRESH_MARKER",
    "ZCODE_CUA_PLUGIN_AUTHORITY",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    "OTEL_SERVICE_NAME",
    "OTEL_RESOURCE_ATTRIBUTES",
    "OTEL_EXPORTER_OTLP_COMPRESSION",
    "ZCODE_MODEL_TELEMETRY_ENABLED",
    "ZCODE_TELEMETRY_DEVICE_MID",
    "ZCODE_TELEMETRY_USER_ID",
    "ZCODE_TELEMETRY_USER_ID_HASH",
    "ZCODE_TELEMETRY_USER_SUBJECT_ID",
    "ZCODE_TELEMETRY_IDENTITY_STATE",
    "ZCODE_TELEMETRY_RUNTIME_SURFACE",
    "ZCODE_TELEMETRY_RUNTIME_DISTRIBUTION",
];

/// `shouldSanitizeZCodeRuntimeEnvKey`：固定清单（不区分大小写）+ 包管理器的代理 / CA 键。
pub fn should_sanitize(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    if SANITIZED_KEYS.contains(&upper.as_str()) {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    let Some((prefix, rest)) = ["npm_config_", "yarn_", "pnpm_"]
        .iter()
        .find_map(|prefix| lower.strip_prefix(prefix).map(|rest| (*prefix, rest)))
    else {
        return false;
    };
    let _ = prefix;
    matches!(
        rest,
        "http_proxy" | "https_proxy" | "proxy" | "all_proxy" | "no_proxy" | "cafile" | "ca"
    )
}

/// 清洗后的 Host 环境 + Main 给的登录 shell 补丁（补丁后写，覆盖同名键）。
pub fn effective_base_env(
    vars: &BTreeMap<String, String>,
    patch: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = vars
        .iter()
        .filter(|(key, _)| !should_sanitize(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for (key, value) in patch {
        // Windows 环境变量名不区分大小写：先去掉大小写不同的同名键，避免出现两个 PATH。
        if cfg!(windows) {
            env.retain(|existing, _| !existing.eq_ignore_ascii_case(key));
        }
        env.insert(key.clone(), value.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_proxy_ca_and_telemetry_keys() {
        assert!(should_sanitize("http_proxy"));
        assert!(should_sanitize("ELECTRON_RUN_AS_NODE"));
        assert!(should_sanitize("npm_config_https_proxy"));
        assert!(should_sanitize("PNPM_CAFILE"));
        assert!(!should_sanitize("npm_config_registry"));
        assert!(!should_sanitize("PATH"));
    }

    #[test]
    fn patch_overrides_after_sanitizing() {
        let vars = BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("NODE_ENV".to_owned(), "development".to_owned()),
        ]);
        let patch = BTreeMap::from([("PATH".to_owned(), "/opt/homebrew/bin:/usr/bin".to_owned())]);
        let env = effective_base_env(&vars, &patch);
        assert_eq!(env["PATH"], "/opt/homebrew/bin:/usr/bin");
        assert!(!env.contains_key("NODE_ENV"));
    }
}
