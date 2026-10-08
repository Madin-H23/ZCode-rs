//! git checkpoint 的 manifest 存储与纯函数（gitCheckpointStore.ts、gitCheckpointHelpers.ts）。
//!
//! manifest 路径、内容格式与 Node 相同（`JSON.stringify(meta, null, 2) + "\n"`），两种 Host 可互读。
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

use super::parse::{normalize_git_path, to_workspace_relative};
use crate::store::atomic::atomic_write_text;
use crate::support::path;

/// `GitCheckpointMeta`；字段顺序与 Node 写出的 manifest 相同。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointMeta {
    pub checkpoint_id: String,
    pub workspace_path: String,
    pub repo_root: String,
    pub workspace_in_repo_path: String,
    pub created_at: f64,
    pub ref_name: String,
    pub commit_oid: String,
    pub scope: String,
}

impl CheckpointMeta {
    /// `isGitCheckpointMeta`：字段类型齐全且 scope 为 workspace。
    fn from_value(value: Value) -> Option<Self> {
        let meta: Self = serde_json::from_value(value).ok()?;
        (meta.scope == "workspace").then_some(meta)
    }

    pub fn to_value(&self) -> Value {
        // createdAt 是整数毫秒；按整数写出，避免 `1.7e12` 一类的浮点表示。
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if self.created_at.fract() == 0.0 && self.created_at.abs() < 9.0e15 {
            value["createdAt"] = json!(self.created_at as i64);
        }
        value
    }
}

/// `getWorkspaceHash(workspacePath)`：sha256 十六进制前 12 位。
pub fn workspace_hash(workspace_key: &str) -> String {
    let digest = Sha256::digest(workspace_key.as_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..12]
        .to_owned()
}

pub fn checkpoint_ref_name(workspace_path: &str, checkpoint_id: &str) -> String {
    format!(
        "refs/zcode/checkpoints/{}/{checkpoint_id}",
        workspace_hash(workspace_path)
    )
}

pub fn workspace_pathspec(workspace_in_repo_path: &str) -> &str {
    if workspace_in_repo_path == "." {
        "."
    } else {
        workspace_in_repo_path
    }
}

pub fn to_absolute(repo_root: &str, repo_relative_path: &str) -> String {
    let normalized = normalize_git_path(repo_relative_path);
    let segments: Vec<&str> = normalized.split('/').collect();
    path::resolve(repo_root, &segments)
}

#[derive(Debug, Clone, PartialEq)]
pub struct NameStatusEntry {
    pub kind: &'static str,
    pub path: String,
    pub original_path: Option<String>,
}

fn name_status_kind(status: &str) -> &'static str {
    match status.chars().next().unwrap_or('M') {
        'A' => "added",
        'D' => "deleted",
        'R' | 'C' => "renamed",
        _ => "modified",
    }
}

fn records(stdout: &str) -> Vec<&str> {
    stdout
        .split('\0')
        .filter(|record| !record.is_empty())
        .collect()
}

/// `parseNameStatus`（`git diff --name-status -z`）。
pub fn parse_name_status(stdout: &str) -> Vec<NameStatusEntry> {
    let records = records(stdout);
    let mut entries = Vec::new();
    let mut index = 0;
    while index < records.len() {
        let kind = name_status_kind(records[index]);
        if kind == "renamed" {
            let original = records.get(index + 1);
            let renamed = records.get(index + 2);
            index += 3;
            if let (Some(original), Some(renamed)) = (original, renamed) {
                entries.push(NameStatusEntry {
                    kind,
                    path: normalize_git_path(renamed),
                    original_path: Some(normalize_git_path(original)),
                });
            }
            continue;
        }
        let file = records.get(index + 1);
        index += 2;
        if let Some(file) = file {
            entries.push(NameStatusEntry {
                kind,
                path: normalize_git_path(file),
                original_path: None,
            });
        }
    }
    entries
}

/// `Number.parseInt(x, 10) || 0`。
fn parse_count(field: &str) -> u64 {
    if field == "-" {
        return 0;
    }
    super::parse::parse_int(field)
        .filter(|value| *value > 0)
        .map(|value| value as u64)
        .unwrap_or(0)
}

/// `parseNumstat`（checkpoint 版）：rename 记录同时登记新旧路径。
pub fn parse_numstat(stdout: &str) -> HashMap<String, (u64, u64)> {
    let records = records(stdout);
    let mut stats = HashMap::new();
    let mut index = 0;
    while index < records.len() {
        let fields: Vec<&str> = records[index].split('\t').collect();
        index += 1;
        if fields.len() < 3 {
            continue;
        }
        let stat = (parse_count(fields[0]), parse_count(fields[1]));
        let path_field = fields[2..].join("\t");
        if !path_field.is_empty() {
            stats.insert(normalize_git_path(&path_field), stat);
            continue;
        }
        let original = records.get(index).copied().unwrap_or("");
        let renamed = records.get(index + 1).copied().unwrap_or("");
        index += 2;
        if renamed.is_empty() {
            continue;
        }
        stats.insert(normalize_git_path(renamed), stat);
        if !original.is_empty() {
            stats.insert(normalize_git_path(original), stat);
        }
    }
    stats
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeEntry {
    pub mode: String,
    pub object_id: String,
}

/// `parseLsTree`（`git ls-tree -r -z`）。
pub fn parse_ls_tree(stdout: &str) -> HashMap<String, TreeEntry> {
    let mut entries = HashMap::new();
    for record in records(stdout) {
        let Some(tab) = record.find('\t') else {
            continue;
        };
        let header: Vec<&str> = record[..tab].split(' ').collect();
        if header.len() < 3 {
            continue;
        }
        entries.insert(
            normalize_git_path(&record[tab + 1..]),
            TreeEntry {
                mode: header[0].to_owned(),
                object_id: header[2].to_owned(),
            },
        );
    }
    entries
}

/// `mergeCheckpointDiff`。
pub fn merge_checkpoint_diff(
    repo_root: &str,
    workspace_in_repo_path: &str,
    (from_id, to_id): (&str, &str),
    entries: &[NameStatusEntry],
    numstat: &HashMap<String, (u64, u64)>,
) -> Value {
    let files: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let (added, removed) = numstat
                .get(&entry.path)
                .or_else(|| numstat.get(entry.original_path.as_deref().unwrap_or("")))
                .copied()
                .unwrap_or((0, 0));
            json!({
                "path": to_absolute(repo_root, &entry.path),
                "repoRelativePath": entry.path,
                "workspaceRelativePath": to_workspace_relative(&entry.path, workspace_in_repo_path),
                "originalPath": entry.original_path.as_deref().map(|original| to_absolute(repo_root, original)),
                "kind": entry.kind,
                "added": added,
                "removed": removed,
            })
        })
        .collect();
    json!({"fromCheckpointId": from_id, "toCheckpointId": to_id, "files": files})
}

/// `normalizeAffectedRepoPath`：绝对路径去掉 `<repoRoot><sep>` 前缀（只替换第一次出现）。
pub fn normalize_affected_repo_path(repo_root: &str, value: &str) -> String {
    if !path::is_absolute(value) {
        return normalize_git_path(value);
    }
    strip_repo_root(repo_root, value, std::path::MAIN_SEPARATOR, cfg!(windows))
}

fn strip_repo_root(repo_root: &str, value: &str, separator: char, windows: bool) -> String {
    let stripped = value.replacen(&format!("{repo_root}{separator}"), "", 1);
    if windows && stripped == value {
        // 修复：Windows 上 git 报告的 repoRoot 是 `C:/repo`，而绝对路径由 resolve 生成为
        // `C:\repo\a`，Node 的字符串替换对不上，rename 旧路径会以绝对路径进入冲突检测。
        // 这里统一成 `/` 并忽略大小写再去前缀（Windows 文件系统大小写不敏感）。
        let root = normalize_git_path(repo_root);
        let prefix = format!("{}/", root.trim_end_matches('/')).to_lowercase();
        let normalized = normalize_git_path(value);
        if normalized.to_lowercase().starts_with(&prefix) {
            return normalized[prefix.len()..].to_owned();
        }
    }
    normalize_git_path(&stripped)
}

/// `GitCheckpointStore`：manifest 放在 `<appConfigDir>/checkpoints/<hash12>/<id>.json`。
/// Node 按文件串行写入、读取先等待写入；checkpoint 操作低频，这里用一把锁串行全部 IO。
pub struct CheckpointStore {
    root: PathBuf,
    io: Mutex<()>,
}

impl CheckpointStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            io: Mutex::new(()),
        }
    }

    fn file(&self, workspace_path: &str, checkpoint_id: &str) -> PathBuf {
        self.root
            .join(workspace_hash(workspace_path))
            .join(format!("{checkpoint_id}.json"))
    }

    pub async fn save(&self, meta: &CheckpointMeta) -> std::io::Result<()> {
        let _guard = self.io.lock().await;
        let file = self.file(&meta.workspace_path, &meta.checkpoint_id);
        let content = format!(
            "{}\n",
            serde_json::to_string_pretty(&meta.to_value()).unwrap_or_default()
        );
        atomic_write_text(&file, &content, false)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))
    }

    /// 读取失败、内容不合法都视为不存在（与 Node 的 `load` 一样返回 null）。
    pub async fn load(&self, workspace_path: &str, checkpoint_id: &str) -> Option<CheckpointMeta> {
        let _guard = self.io.lock().await;
        let raw = tokio::fs::read_to_string(self.file(workspace_path, checkpoint_id))
            .await
            .ok()?;
        CheckpointMeta::from_value(serde_json::from_str(&raw).ok()?)
    }

    pub async fn delete(&self, workspace_path: &str, checkpoint_id: &str) -> std::io::Result<()> {
        let _guard = self.io.lock().await;
        match tokio::fs::remove_file(self.file(workspace_path, checkpoint_id)).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// `rm(path, { recursive: true, force: true })`。
pub async fn remove_if_exists(target: &Path) -> std::io::Result<()> {
    let Ok(meta) = tokio::fs::symlink_metadata(target).await else {
        return Ok(());
    };
    let result = if meta.is_dir() {
        tokio::fs::remove_dir_all(target).await
    } else {
        tokio::fs::remove_file(target).await
    };
    match result {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_hash_matches_node() {
        // node -e 'console.log(require("crypto").createHash("sha256").update("/tmp/ws").digest("hex").slice(0,12))'
        assert_eq!(workspace_hash("/tmp/ws"), "b011ea26cc73");
        assert_eq!(
            checkpoint_ref_name("/tmp/ws", "id-1"),
            "refs/zcode/checkpoints/b011ea26cc73/id-1"
        );
    }

    #[test]
    fn parses_name_status_numstat_and_ls_tree() {
        let entries = parse_name_status("M\0a.txt\0R100\0old.txt\0new.txt\0D\0gone.txt\0");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].kind, "renamed");
        assert_eq!(entries[1].original_path.as_deref(), Some("old.txt"));
        assert_eq!(entries[2].kind, "deleted");
        let stats = parse_numstat("3\t1\ta.txt\0");
        assert_eq!(stats.get("a.txt"), Some(&(3, 1)));
        let renamed = parse_numstat("0\t0\t\0old.txt\0new.txt\0-\t-\tbin.png\0");
        assert_eq!(renamed.get("new.txt"), Some(&(0, 0)));
        assert_eq!(renamed.get("old.txt"), Some(&(0, 0)));
        assert_eq!(renamed.get("bin.png"), Some(&(0, 0)));
        let tree = parse_ls_tree("100644 blob abc123\tsrc/a.txt\x00120000 blob def\tlink\x00");
        assert_eq!(tree["src/a.txt"].object_id, "abc123");
        assert_eq!(tree["link"].mode, "120000");
    }

    #[test]
    fn affected_paths_strip_repo_root_on_both_platforms() {
        assert_eq!(
            strip_repo_root("/repo", "/repo/src/a.ts", '/', false),
            "src/a.ts"
        );
        assert_eq!(
            strip_repo_root("C:/Repo", r"C:\repo\src\a.ts", '\\', true),
            "src/a.ts"
        );
        assert_eq!(
            strip_repo_root(r"C:\Repo", r"C:\Repo\src\a.ts", '\\', true),
            "src/a.ts"
        );
    }

    #[test]
    fn manifest_round_trips_in_node_format() {
        let meta = CheckpointMeta {
            checkpoint_id: "c1".into(),
            workspace_path: "/w".into(),
            repo_root: "/w".into(),
            workspace_in_repo_path: ".".into(),
            created_at: 1_700_000_000_000.0,
            ref_name: "refs/zcode/checkpoints/x/c1".into(),
            commit_oid: "abc".into(),
            scope: "workspace".into(),
        };
        let text = serde_json::to_string(&meta.to_value()).unwrap();
        assert_eq!(
            text,
            r#"{"checkpointId":"c1","workspacePath":"/w","repoRoot":"/w","workspaceInRepoPath":".","createdAt":1700000000000,"refName":"refs/zcode/checkpoints/x/c1","commitOid":"abc","scope":"workspace"}"#
        );
        let mut other = meta.to_value();
        other["scope"] = json!("repo");
        assert!(CheckpointMeta::from_value(other).is_none());
        assert_eq!(CheckpointMeta::from_value(meta.to_value()), Some(meta));
    }
}
