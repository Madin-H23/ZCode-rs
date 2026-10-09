//! 远端目标与身份工具（`packages/shared/src/remoteTarget.ts`、`remote-workspace-identity.ts`、
//! `remoteEnvironmentKey.ts`、`remoteSshHostKey.ts`）。与 TS 逐字节相同；业务代码只经这里构造身份，
//! 不手写拼接规则。解析仍用 `zcode_cli_domain::workspace_identity::parse_remote`。
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Ssh,
    Wsl,
    Docker,
}

/// Main 已用 `remoteTargetSchema` 校验过的目标；原文（含密码、口令）只交给连接器。
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTarget {
    raw: Value,
    kind: TargetKind,
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn trimmed<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    text(value, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

impl RemoteTarget {
    pub fn parse(raw: &Value) -> Result<Self, String> {
        let kind = match text(raw, "kind") {
            Some("ssh") => {
                if trimmed(raw, "host").is_none() || trimmed(raw, "username").is_none() {
                    return Err("ssh target requires host and username".into());
                }
                TargetKind::Ssh
            }
            Some("wsl") => TargetKind::Wsl,
            Some("docker") => {
                if trimmed(raw, "container").is_none() {
                    return Err("docker target requires container".into());
                }
                TargetKind::Docker
            }
            other => return Err(format!("unsupported remote target kind {other:?}")),
        };
        Ok(Self {
            raw: raw.clone(),
            kind,
        })
    }

    pub fn kind(&self) -> TargetKind {
        self.kind
    }

    pub fn kind_str(&self) -> &'static str {
        match self.kind {
            TargetKind::Ssh => "ssh",
            TargetKind::Wsl => "wsl",
            TargetKind::Docker => "docker",
        }
    }

    /// 含凭据的原文，只用于建连。
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    fn port(&self) -> u64 {
        self.raw.get("port").and_then(Value::as_u64).unwrap_or(22)
    }

    /// `stripRemoteTargetSecrets`：长期状态与跨进程回包只用这一份。
    pub fn sanitized(&self) -> Value {
        let mut value = self.raw.clone();
        if self.kind == TargetKind::Ssh
            && let Some(object) = value.as_object_mut()
        {
            object.shift_remove("password");
            object.shift_remove("privateKeyPassphrase");
        }
        value
    }

    /// `formatRemoteTargetForLog`：不含凭据。
    pub fn log_label(&self) -> String {
        match self.kind {
            TargetKind::Ssh => format!(
                "ssh:{}@{}:{}",
                text(&self.raw, "username").unwrap_or_default(),
                text(&self.raw, "host").unwrap_or_default(),
                self.port()
            ),
            TargetKind::Wsl => {
                let distro = text(&self.raw, "distro").unwrap_or("default");
                match trimmed(&self.raw, "user") {
                    Some(user) => format!("wsl:{distro}:{user}"),
                    None => format!("wsl:{distro}"),
                }
            }
            TargetKind::Docker => format!(
                "docker:{}",
                text(&self.raw, "container").unwrap_or_default()
            ),
        }
    }

    /// `buildSshRemoteHostKey`。
    pub fn ssh_host_key(&self) -> String {
        let auth = if trimmed(&self.raw, "privateKeyPath").is_some() {
            "private-key"
        } else if self.raw.get("password").is_some_and(Value::is_string) {
            "password"
        } else {
            "agent"
        };
        json!([
            "ssh:v1",
            text(&self.raw, "host")
                .unwrap_or_default()
                .trim()
                .to_lowercase(),
            self.port(),
            text(&self.raw, "username").unwrap_or_default().trim(),
            auth,
            normalize_private_key_path(text(&self.raw, "privateKeyPath")),
        ])
        .to_string()
    }

    /// `buildRemoteEnvironmentKey`。
    pub fn environment_key(&self) -> String {
        match self.kind {
            TargetKind::Ssh => format!("ssh:{}", self.ssh_host_key()),
            TargetKind::Wsl => format!(
                "wsl:{}\0{}",
                trimmed(&self.raw, "distro").unwrap_or("<default>"),
                trimmed(&self.raw, "user").unwrap_or("<default>")
            ),
            TargetKind::Docker => format!(
                "docker:{}",
                text(&self.raw, "container").unwrap_or_default().trim()
            ),
        }
    }

    /// 窗口 registry 的连接复用键（`buildConnectionKey`）：Docker 不复用。
    pub fn connection_key(&self, remote_session_id: &str) -> String {
        match self.kind {
            TargetKind::Ssh => format!("ssh:{}", self.ssh_host_key()),
            TargetKind::Wsl => format!(
                "wsl:{}\0{}",
                trimmed(&self.raw, "distro").unwrap_or("<default>"),
                trimmed(&self.raw, "user").unwrap_or("<default>")
            ),
            TargetKind::Docker => format!("docker:dedicated:{remote_session_id}"),
        }
    }

    /// `isSameRemoteTarget`（desktopRemoteSessions.ts）：同一台机器上的同一身份，凭据不参与比较。
    pub fn same_as(&self, other: &Value) -> bool {
        let left = &self.raw;
        match self.kind {
            TargetKind::Ssh => {
                text(other, "kind") == Some("ssh")
                    && trimmed_text_of(left, "host").to_lowercase()
                        == trimmed_text_of(other, "host").to_lowercase()
                    && self.port() == other.get("port").and_then(Value::as_u64).unwrap_or(22)
                    && trimmed_text_of(left, "username") == trimmed_text_of(other, "username")
                    && text(left, "privateKeyPath").unwrap_or_default()
                        == text(other, "privateKeyPath").unwrap_or_default()
            }
            TargetKind::Wsl => {
                text(other, "kind") == Some("wsl")
                    && trimmed(left, "distro").unwrap_or("default")
                        == trimmed(other, "distro").unwrap_or("default")
                    && trimmed_text_of(left, "user") == trimmed_text_of(other, "user")
            }
            TargetKind::Docker => {
                text(other, "kind") == Some("docker")
                    && text(left, "container") == text(other, "container")
            }
        }
    }

    /// `buildRemoteWorkspaceIdentity`。
    pub fn workspace_identity(&self, workspace_path: &str) -> String {
        let path = normalize_identity_path(workspace_path);
        match self.kind {
            TargetKind::Ssh => format!(
                "remote:ssh:{}:{}:{}:{path}",
                text(&self.raw, "host")
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase(),
                self.port(),
                text(&self.raw, "username").unwrap_or_default().trim()
            ),
            TargetKind::Wsl => {
                let distro = trimmed(&self.raw, "distro").unwrap_or("default");
                match trimmed(&self.raw, "user") {
                    Some(user) => format!("remote:wsl:{distro}:{user}:{path}"),
                    None => format!("remote:wsl:{distro}:{path}"),
                }
            }
            TargetKind::Docker => format!(
                "remote:docker:{}:{path}",
                text(&self.raw, "container").unwrap_or_default()
            ),
        }
    }
}

/// 去掉首尾空白的字符串字段；缺失为空串。
fn trimmed_text_of<'a>(value: &'a Value, key: &str) -> &'a str {
    text(value, key).map(str::trim).unwrap_or_default()
}

/// `normalizeWorkspacePathForIdentity`：分隔符归一、折叠重复斜杠、去首尾斜杠，恒以 "/" 开头。
fn normalize_identity_path(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let mut collapsed = String::with_capacity(unified.len());
    for character in unified.chars() {
        if character == '/' && collapsed.ends_with('/') {
            continue;
        }
        collapsed.push(character);
    }
    format!("/{}", collapsed.trim_matches('/'))
}

/// `normalizePrivateKeyPath`（remoteSshHostKey.ts）。
fn normalize_private_key_path(value: Option<&str>) -> String {
    let Some(trimmed) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return String::new();
    };
    let mut normalized = trimmed.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive = (bytes[0] as char).to_ascii_uppercase();
        normalized.replace_range(0..1, &drive.to_string());
    }
    let (prefix, body, blocks_parent) = split_private_key_root(&normalized);
    let mut segments: Vec<&str> = Vec::new();
    for segment in body.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            if segments.last().is_some_and(|last| *last != "..") {
                segments.pop();
                continue;
            }
            if blocks_parent {
                continue;
            }
        }
        segments.push(segment);
    }
    let joined = format!("{prefix}{}", segments.join("/"));
    if joined.is_empty() { prefix } else { joined }
}

/// `splitPrivateKeyPathRoot`：返回 (前缀, 主体, 根是否阻止 `..` 越界)。
fn split_private_key_root(value: &str) -> (String, String, bool) {
    let bytes = value.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_uppercase() && bytes[1] == b':';
    if drive && bytes.get(2) == Some(&b'/') {
        return (value[..3].to_owned(), value[3..].to_owned(), true);
    }
    if let Some(rest) = value.strip_prefix("//") {
        let segments: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
        if segments.len() >= 2 {
            return (
                format!("//{}/{}/", segments[0], segments[1]),
                segments[2..].join("/"),
                true,
            );
        }
        return ("//".into(), segments.join("/"), true);
    }
    if let Some(rest) = value.strip_prefix('/') {
        return ("/".into(), rest.trim_start_matches('/').to_owned(), true);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return ("~/".into(), rest.to_owned(), false);
    }
    if drive {
        return (value[..2].to_owned(), value[2..].to_owned(), false);
    }
    (String::new(), value.to_owned(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期望值由 TS 实现生成（buildRemoteWorkspaceIdentity / buildRemoteEnvironmentKey /
    /// buildSshRemoteHostKey），见 docs/specs/rust-host-remote.md §7。
    #[test]
    fn identities_match_typescript() {
        let paths = ["/home/dev/proj/", "C:\\work\\x", "", "//a//b"];
        let cases: &[(Value, &str, &[&str])] = &[
            (
                json!({"kind":"ssh","host":" Example.COM ","username":" dev ","password":"x"}),
                "ssh:[\"ssh:v1\",\"example.com\",22,\"dev\",\"password\",\"\"]",
                &[
                    "remote:ssh:example.com:22:dev:/home/dev/proj",
                    "remote:ssh:example.com:22:dev:/C:/work/x",
                    "remote:ssh:example.com:22:dev:/",
                    "remote:ssh:example.com:22:dev:/a/b",
                ],
            ),
            (
                json!({"kind":"ssh","host":"h","port":2222,"username":"u","privateKeyPath":"c:\\Users\\me\\.ssh\\..\\id_rsa"}),
                "ssh:[\"ssh:v1\",\"h\",2222,\"u\",\"private-key\",\"C:/Users/me/id_rsa\"]",
                &["remote:ssh:h:2222:u:/home/dev/proj"],
            ),
            (
                json!({"kind":"ssh","host":"h","username":"u","privateKeyPath":"~/.ssh/./keys/../id_ed25519"}),
                "ssh:[\"ssh:v1\",\"h\",22,\"u\",\"private-key\",\"~/.ssh/id_ed25519\"]",
                &[],
            ),
            (
                json!({"kind":"ssh","host":"h","username":"u","privateKeyPath":"//server/share/a/../../b"}),
                "ssh:[\"ssh:v1\",\"h\",22,\"u\",\"private-key\",\"//server/share/b\"]",
                &[],
            ),
            (
                json!({"kind":"ssh","host":"h","username":"u","privateKeyPath":"/a/../../b/"}),
                "ssh:[\"ssh:v1\",\"h\",22,\"u\",\"private-key\",\"/b\"]",
                &[],
            ),
            (
                json!({"kind":"ssh","host":"h","username":"u","privateKeyPath":"../x"}),
                "ssh:[\"ssh:v1\",\"h\",22,\"u\",\"private-key\",\"../x\"]",
                &[],
            ),
            (
                json!({"kind":"ssh","host":"h","username":"u"}),
                "ssh:[\"ssh:v1\",\"h\",22,\"u\",\"agent\",\"\"]",
                &[],
            ),
            (
                json!({"kind":"wsl"}),
                "wsl:<default>\0<default>",
                &[
                    "remote:wsl:default:/home/dev/proj",
                    "remote:wsl:default:/C:/work/x",
                    "remote:wsl:default:/",
                    "remote:wsl:default:/a/b",
                ],
            ),
            (
                json!({"kind":"wsl","distro":" Ubuntu ","user":" me "}),
                "wsl:Ubuntu\0me",
                &["remote:wsl:Ubuntu:me:/home/dev/proj"],
            ),
            (
                json!({"kind":"docker","container":" c1 "}),
                "docker:c1",
                &["remote:docker: c1 :/home/dev/proj"],
            ),
        ];
        for (raw, environment, identities) in cases {
            let target = RemoteTarget::parse(raw).unwrap();
            assert_eq!(target.environment_key(), *environment, "{raw}");
            for (path, expected) in paths.iter().zip(identities.iter()) {
                assert_eq!(target.workspace_identity(path), *expected, "{raw} {path}");
                assert!(
                    zcode_cli_domain::workspace_identity::parse_remote(expected).is_some(),
                    "parser accepts {expected}"
                );
            }
        }
    }

    #[test]
    fn secrets_never_leave_the_raw_target() {
        let target = RemoteTarget::parse(&json!({
            "kind":"ssh","host":"h","username":"u","password":"p","privateKeyPassphrase":"q","port":2200
        }))
        .unwrap();
        assert_eq!(
            target.sanitized(),
            json!({"kind":"ssh","host":"h","username":"u","port":2200})
        );
        assert_eq!(target.log_label(), "ssh:u@h:2200");
        let key = target.connection_key("s");
        assert!(!key.contains("\"p\"") && !key.contains("\"q\""), "{key}");
        let docker = RemoteTarget::parse(&json!({"kind":"docker","container":"c"})).unwrap();
        assert_eq!(docker.connection_key("s1"), "docker:dedicated:s1");
        let wsl = RemoteTarget::parse(&json!({"kind":"wsl","distro":"D"})).unwrap();
        assert_eq!(wsl.connection_key("s1"), "wsl:D\0<default>");
        assert!(RemoteTarget::parse(&json!({"kind":"server","url":"x"})).is_err());
    }
}
