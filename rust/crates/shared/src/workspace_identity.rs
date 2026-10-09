//! Remote workspace identities (Node `packages/shared/src/remote-workspace-identity.ts`):
//! `remote:ssh:<host>:<port>:<user>:<path>`, `remote:wsl:<distro>[:<user>]:<path>`,
//! `remote:docker:<container>:<path>`. The path always starts with `/`.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// A workspace as requests and sessions name it (Node `ZCodeWorkspaceRef`):
/// `key` (`workspaceIdentity?.trim() || workspacePath`) isolates sessions,
/// lists and topics; `path` roots file operations and command cwd.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub key: String,
    pub path: String,
}

impl Workspace {
    /// Node `buildWorkspaceRef`: a blank identity falls back to the path.
    pub fn new(identity: Option<&str>, path: impl Into<String>) -> Self {
        let path = path.into();
        let key = identity
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .map_or_else(|| path.clone(), str::to_owned);
        Self { key, path }
    }

    /// The `workspace` field of a request (`None` when absent or null).
    pub fn from_ref(value: Option<&Value>) -> Result<Option<Self>> {
        let Some(value) = value.filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let path = value["workspacePath"]
            .as_str()
            .filter(|p| !p.is_empty())
            .context("Workspace required")?;
        Ok(Some(Self::new(value["workspaceIdentity"].as_str(), path)))
    }

    /// Node `resolveWorkspaceRefFromId`: a V4 workspace id is either a remote
    /// identity (its path parsed out) or a local path.
    pub fn from_id(id: &str) -> Result<Self> {
        if id.is_empty() {
            bail!("Workspace required");
        }
        match parse_remote(id) {
            Some(remote) => Ok(Self::new(Some(id), remote.path)),
            // remote 命名空间必须 fail-closed：非法 identity 不能被当成本地路径写进 directory/path。
            None if id.starts_with("remote:") => bail!("Invalid remote workspace identity: {id}"),
            None => Ok(Self::new(None, id)),
        }
    }

    /// The identity when the key is not the path itself.
    pub fn identity(&self) -> Option<&str> {
        (self.key != self.path).then_some(self.key.as_str())
    }

    /// The protocol ref `{workspacePath, workspaceKey, workspaceIdentity?}`.
    pub fn to_ref(&self) -> Value {
        let mut value = json!({"workspacePath": self.path, "workspaceKey": self.key});
        if let Some(identity) = self.identity() {
            value["workspaceIdentity"] = identity.into();
        }
        value
    }
}

/// A local workspace: its path is its key.
impl From<String> for Workspace {
    fn from(path: String) -> Self {
        Self::new(None, path)
    }
}

impl From<&str> for Workspace {
    fn from(path: &str) -> Self {
        Self::new(None, path)
    }
}

/// A parsed remote identity: its kind and the remote workspace path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remote<'a> {
    pub kind: &'static str,
    pub path: &'a str,
}

/// Node `parseRemoteWorkspaceIdentity`; `None` for local or malformed identities.
pub fn parse_remote(identity: &str) -> Option<Remote<'_>> {
    let rest = identity.strip_prefix("remote:")?;
    let kind_end = rest.find(':').filter(|end| *end > 0)?;
    let (kind, segments) = match &rest[..kind_end] {
        "ssh" => ("ssh", 3),
        "wsl" => ("wsl", 1),
        "docker" => ("docker", 1),
        _ => return None,
    };
    // authority 逐段消费：路径段本身可能含 ":"，不能整体 split。
    let mut cursor = kind_end + 1;
    for _ in 0..segments {
        let next = cursor + rest[cursor..].find(':').filter(|offset| *offset > 0)?;
        cursor = next + 1;
    }
    if kind == "wsl" && !rest[cursor..].starts_with('/') {
        let user_end = cursor + rest[cursor..].find(':').filter(|offset| *offset > 0)?;
        cursor = user_end + 1;
    }
    let path = &rest[cursor..];
    path.starts_with('/').then_some(Remote { kind, path })
}

#[cfg(test)]
#[path = "workspace_identity_tests.rs"]
mod tests;
