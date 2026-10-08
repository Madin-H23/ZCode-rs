//! 子进程的基础环境（packages/services/src/runtime-tools/runtimeCommandEnv.ts:240-267）。
//!
//! TS Host 启动时就地改写全局 `process.env`：Windows 先把各种大小写的 PATH 合成一个，再删掉
//! 运行时敏感变量（packages/shared/src/runtimeEnv.ts:43-110,268-292），最后叠加 Main 采集的登录
//! shell 补丁。Rust Host 不改自身环境，而是算出同样一份快照，git、terminal 等子进程都以它为底。
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

/// `shouldSanitizeZCodeRuntimeEnvKey`：名单按大写比较，包管理器代理/证书键不区分大小写。
fn is_sanitized(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    if SANITIZED_KEYS.contains(&upper.as_str()) {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    ["npm_config_", "yarn_", "pnpm_"].iter().any(|prefix| {
        lower.strip_prefix(prefix).is_some_and(|rest| {
            [
                "http_proxy",
                "https_proxy",
                "proxy",
                "all_proxy",
                "no_proxy",
                "cafile",
                "ca",
            ]
            .contains(&rest)
        })
    })
}

/// 子进程环境；Windows 上变量名不区分大小写（与 Node 的 `process.env` 一致）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessEnv {
    vars: BTreeMap<String, String>,
    ignore_case: bool,
}

impl ProcessEnv {
    pub fn new(vars: BTreeMap<String, String>) -> Self {
        Self::with_case(vars, cfg!(windows))
    }

    fn with_case(vars: BTreeMap<String, String>, ignore_case: bool) -> Self {
        let mut env = Self {
            vars: BTreeMap::new(),
            ignore_case,
        };
        for (key, value) in vars {
            env.set(key, value);
        }
        env
    }

    /// Host 子进程的基础环境：清理敏感变量后叠加 Main 的登录 shell 补丁。
    pub fn for_children(vars: &BTreeMap<String, String>, patch: &BTreeMap<String, String>) -> Self {
        Self::build(vars, patch, cfg!(windows))
    }

    fn build(
        vars: &BTreeMap<String, String>,
        patch: &BTreeMap<String, String>,
        ignore_case: bool,
    ) -> Self {
        let kept = vars
            .iter()
            .filter(|(key, _)| !is_sanitized(key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let mut env = Self::with_case(kept, ignore_case);
        // Windows：继承的 Path 与补丁的 PATH 必须合成一个键，否则子进程只会保留其中一项。
        if ignore_case && let Some(path) = env.remove("PATH") {
            env.vars.insert("PATH".to_owned(), path);
        }
        for (key, value) in patch {
            env.set(key.clone(), value.clone());
        }
        env
    }

    fn matches(&self, candidate: &str, key: &str) -> bool {
        if self.ignore_case {
            candidate.eq_ignore_ascii_case(key)
        } else {
            candidate == key
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        if !self.ignore_case {
            return self.vars.get(key).map(String::as_str);
        }
        self.vars
            .iter()
            .rev()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_str())
    }

    /// 去掉首尾空白后非空的值。
    pub fn non_empty(&self, key: &str) -> Option<&str> {
        self.get(key)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        if self.ignore_case {
            self.remove(&key);
        }
        self.vars.insert(key, value.into());
    }

    /// 删除（Windows 上所有大小写形式）并返回最后一个值。
    pub fn remove(&mut self, key: &str) -> Option<String> {
        let keys: Vec<String> = self
            .vars
            .keys()
            .filter(|candidate| self.matches(candidate, key))
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|key| self.vars.remove(&key))
            .last()
    }

    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    /// `PATH` 按平台分隔符拆分后的目录。
    pub fn path_dirs(&self) -> Vec<String> {
        let delimiter = if cfg!(windows) { ';' } else { ':' };
        self.get("PATH")
            .map(|path| path.split(delimiter).map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn sanitizes_runtime_keys_and_applies_the_patch() {
        let env = ProcessEnv::build(
            &map(&[
                ("PATH", "/usr/bin"),
                ("http_proxy", "http://proxy"),
                ("ELECTRON_RUN_AS_NODE", "1"),
                ("npm_config_cafile", "/ca.pem"),
                ("npm_config_cache", "/cache"),
                ("HOME", "/home/u"),
            ]),
            &map(&[("PATH", "/opt/bin:/usr/bin")]),
            false,
        );
        assert_eq!(env.get("PATH"), Some("/opt/bin:/usr/bin"));
        assert_eq!(env.get("http_proxy"), None);
        assert_eq!(env.get("ELECTRON_RUN_AS_NODE"), None);
        assert_eq!(env.get("npm_config_cafile"), None);
        assert_eq!(env.get("npm_config_cache"), Some("/cache"));
        assert_eq!(env.get("HOME"), Some("/home/u"));
    }

    #[test]
    fn windows_mode_merges_path_keys_case_insensitively() {
        let env = ProcessEnv::build(
            &map(&[("Path", "C:\\Windows"), ("ComSpec", "C:\\cmd.exe")]),
            &map(&[("PATH", "C:\\Git\\cmd;C:\\Windows")]),
            true,
        );
        assert_eq!(env.vars().len(), 2);
        assert_eq!(env.get("path"), Some("C:\\Git\\cmd;C:\\Windows"));
        assert_eq!(env.get("COMSPEC"), Some("C:\\cmd.exe"));
    }
}
