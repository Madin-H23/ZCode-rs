//! `git-checkpoint` channel（gitCheckpointService.ts、repo/gitCheckpointRepo.ts）。
//!
//! 用临时 `GIT_INDEX_FILE` 把 workspace 的 live 状态固化成隐藏 commit，挂在
//! `refs/zcode/checkpoints/<hash12>/<id>` 上；manifest 由 [`CheckpointStore`] 保存。
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use super::checkpoint_store::{
    CheckpointMeta, CheckpointStore, NameStatusEntry, checkpoint_ref_name, merge_checkpoint_diff,
    normalize_affected_repo_path, parse_ls_tree, parse_name_status, parse_numstat,
    remove_if_exists, to_absolute, workspace_pathspec,
};
use super::command::{GitRunner, Limits, git_error};
use super::parse::to_workspace_relative;
use super::repo::{GitRepo, Resolution};

const CHECKPOINT_ENV: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "ZCode Checkpoint"),
    ("GIT_AUTHOR_EMAIL", "checkpoint@zcode.local"),
    ("GIT_COMMITTER_NAME", "ZCode Checkpoint"),
    ("GIT_COMMITTER_EMAIL", "checkpoint@zcode.local"),
];

#[derive(Clone)]
pub struct Checkpoints {
    inner: Arc<Inner>,
}

struct Inner {
    repo: GitRepo,
    store: CheckpointStore,
    /// `<dataBase>/.zcode/git-checkpoint-index`
    index_root: PathBuf,
}

fn io_error(error: std::io::Error) -> RpcError {
    git_error(error.to_string())
}

/// 一次 checkpoint 间 diff 的结构化结果（`GitCheckpointDiff.files`）。
struct DiffFile {
    path: String,
    repo_relative_path: String,
    original_path: Option<String>,
    kind: &'static str,
}

fn diff_files(diff: &Value) -> Vec<DiffFile> {
    diff["files"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|file| DiffFile {
            path: file["path"].as_str().unwrap_or_default().to_owned(),
            repo_relative_path: file["repoRelativePath"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            original_path: file["originalPath"].as_str().map(str::to_owned),
            kind: match file["kind"].as_str() {
                Some("added") => "added",
                Some("deleted") => "deleted",
                Some("renamed") => "renamed",
                _ => "modified",
            },
        })
        .collect()
}

/// 临时 index 目录（`mkdtemp(<root>/index-)`）。
async fn make_temp_dir(root: &Path) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(root).await?;
    loop {
        let suffix: String = crate::main_bridge::random_id().chars().take(6).collect();
        let dir = root.join(format!("index-{suffix}"));
        match tokio::fs::create_dir(&dir).await {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            other => return other.map(|()| dir),
        }
    }
}

impl Checkpoints {
    /// manifest IO 的 owner 任务在 `spawner` 的作用域里。
    pub fn new(
        spawner: &zcode_async::Spawner,
        repo: GitRepo,
        data_root: PathBuf,
        app_config_dir: PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                repo,
                store: CheckpointStore::new(spawner, app_config_dir.join("checkpoints")),
                index_root: data_root.join("git-checkpoint-index"),
            }),
        }
    }

    fn git(&self) -> &GitRunner {
        self.inner.repo.runner()
    }

    /// `ensureRepository`：所有 checkpoint 操作都要求 workspace 在真实 Git 仓库里。
    async fn ensure_repository(&self, workspace_path: &str) -> Result<Arc<Resolution>, RpcError> {
        let resolution = self.inner.repo.resolve(workspace_path).await?;
        if !resolution.is_git_available {
            return Err(git_error(
                "Git binary is not available in the current environment.",
            ));
        }
        if !resolution.is_repository {
            return Err(git_error("Workspace is not inside a Git repository."));
        }
        Ok(resolution)
    }

    async fn load(
        &self,
        workspace_path: &str,
        checkpoint_id: &str,
    ) -> Result<CheckpointMeta, RpcError> {
        let meta = self
            .inner
            .store
            .load(workspace_path, checkpoint_id)
            .await
            .ok_or_else(|| git_error(format!("Checkpoint does not exist: {checkpoint_id}")))?;
        if meta.workspace_path != workspace_path {
            return Err(git_error(format!(
                "Checkpoint workspace mismatch: {}",
                meta.checkpoint_id
            )));
        }
        Ok(meta)
    }

    async fn load_pair(
        &self,
        workspace_path: &str,
        from_id: &str,
        to_id: &str,
    ) -> Result<(CheckpointMeta, CheckpointMeta), RpcError> {
        let (from, to) = tokio::join!(
            self.load(workspace_path, from_id),
            self.load(workspace_path, to_id)
        );
        let (from, to) = (from?, to?);
        if from.repo_root != to.repo_root {
            return Err(git_error("Checkpoint repoRoot mismatch."));
        }
        if from.scope != to.scope {
            return Err(git_error("Checkpoint scope mismatch."));
        }
        if from.workspace_in_repo_path != to.workspace_in_repo_path {
            return Err(git_error("Checkpoint workspace scope mismatch."));
        }
        Ok((from, to))
    }

    pub async fn create(&self, workspace_path: &str) -> Result<Value, RpcError> {
        let checkpoint_id = crate::agent::service::uuid_v4();
        let meta = self.create_snapshot(workspace_path, &checkpoint_id).await?;
        self.inner.store.save(&meta).await.map_err(io_error)?;
        Ok(meta.to_value())
    }

    async fn create_snapshot(
        &self,
        workspace_path: &str,
        checkpoint_id: &str,
    ) -> Result<CheckpointMeta, RpcError> {
        let resolution = self.ensure_repository(workspace_path).await?;
        let ref_name = checkpoint_ref_name(workspace_path, checkpoint_id);
        let temp_dir = make_temp_dir(&self.inner.index_root)
            .await
            .map_err(io_error)?;
        let result = self
            .snapshot_in(&resolution, &temp_dir, checkpoint_id, &ref_name)
            .await;
        // 临时 index 只服务于本次构建，结束后必须清掉。
        let _ = remove_if_exists(&temp_dir).await;
        let commit_oid = result?;
        Ok(CheckpointMeta {
            checkpoint_id: checkpoint_id.to_owned(),
            workspace_path: workspace_path.to_owned(),
            repo_root: resolution.repo_root.clone(),
            workspace_in_repo_path: resolution.workspace_in_repo_path.clone(),
            created_at: now_ms(),
            ref_name,
            commit_oid,
            scope: "workspace".to_owned(),
        })
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or_default()
}

impl Checkpoints {
    /// 临时 index → `add -A` → `write-tree` → `commit-tree` → `update-ref`；返回 commit oid。
    async fn snapshot_in(
        &self,
        resolution: &Resolution,
        temp_dir: &Path,
        checkpoint_id: &str,
        ref_name: &str,
    ) -> Result<String, RpcError> {
        let repo_root = resolution.repo_root.as_str();
        let index_path = temp_dir.join("index");
        let index_text = index_path.to_string_lossy().into_owned();
        let mut env: Vec<(&str, &str)> = vec![("GIT_INDEX_FILE", index_text.as_str())];
        env.extend(CHECKPOINT_ENV);
        let pathspec = workspace_pathspec(&resolution.workspace_in_repo_path);
        let git = self.git();

        // 预热临时 index：优先复制用户真实 index（复用 stat cache，大仓库 add 从数秒降到亚秒），
        // 降级到 read-tree HEAD，最差回到空 index；最终 tree 由 worktree 决定，语义等价。
        let mut primed = false;
        let index_location = git
            .run(
                repo_root,
                &["rev-parse", "--git-path", "index"],
                Limits::COMMAND,
            )
            .await?;
        if index_location.succeeded() {
            let relative = index_location.stdout.trim();
            if !relative.is_empty() {
                let user_index = crate::support::path::resolve(repo_root, &[relative]);
                primed = tokio::fs::copy(&user_index, &index_path).await.is_ok();
            }
        }
        if !primed {
            // 全新仓库没有 HEAD，read-tree 失败时静默回落到空 index。
            let _ = git
                .run_with_env(repo_root, &["read-tree", "HEAD"], Limits::COMMAND, &env)
                .await?;
        }
        git.run_with_env(
            repo_root,
            &["add", "-A", "--", pathspec],
            Limits::COMMAND,
            &env,
        )
        .await?
        .ensure("git add checkpoint")?;
        let tree = git
            .run_with_env(repo_root, &["write-tree"], Limits::COMMAND, &env)
            .await?;
        tree.ensure("git write-tree checkpoint")?;
        let message = format!("zcode checkpoint {checkpoint_id}");
        let commit = git
            .run_with_env(
                repo_root,
                &["commit-tree", tree.stdout.trim(), "-m", &message],
                Limits::COMMAND,
                &env,
            )
            .await?;
        commit.ensure("git commit-tree checkpoint")?;
        let commit_oid = commit.stdout.trim().to_owned();
        git.run(
            repo_root,
            &["update-ref", ref_name, &commit_oid],
            Limits::COMMAND,
        )
        .await?
        .ensure("git update-ref checkpoint")?;
        Ok(commit_oid)
    }

    /// checkpoint 之间的 diff 只针对当前 workspace scope。
    async fn compute_diff(
        &self,
        workspace_path: &str,
        from: &CheckpointMeta,
        to: &CheckpointMeta,
    ) -> Result<(Arc<Resolution>, Value), RpcError> {
        let resolution = self.ensure_repository(workspace_path).await?;
        let repo_root = resolution.repo_root.as_str();
        let pathspec = workspace_pathspec(&resolution.workspace_in_repo_path);
        let args = |mode: &'static str| {
            [
                "diff",
                mode,
                "--find-renames",
                "-z",
                from.commit_oid.as_str(),
                to.commit_oid.as_str(),
                "--",
                pathspec,
            ]
        };
        let (name_status_args, numstat_args) = (args("--name-status"), args("--numstat"));
        let (name_status, numstat) = tokio::join!(
            self.git()
                .run(repo_root, &name_status_args, Limits::COMMAND),
            self.git().run(repo_root, &numstat_args, Limits::COMMAND)
        );
        let (name_status, numstat) = (name_status?, numstat?);
        name_status.ensure("git diff --name-status checkpoint")?;
        numstat.ensure("git diff --numstat checkpoint")?;
        let entries: Vec<NameStatusEntry> = parse_name_status(&name_status.stdout);
        let diff = merge_checkpoint_diff(
            repo_root,
            &resolution.workspace_in_repo_path,
            (&from.checkpoint_id, &to.checkpoint_id),
            &entries,
            &parse_numstat(&numstat.stdout),
        );
        Ok((resolution, diff))
    }

    pub async fn diff(&self, params: &PairParams) -> Result<Value, RpcError> {
        let (from, to) = self
            .load_pair(
                &params.workspace_path,
                &params.from_checkpoint_id,
                &params.to_checkpoint_id,
            )
            .await?;
        Ok(self
            .compute_diff(&params.workspace_path, &from, &to)
            .await?
            .1)
    }

    pub async fn delete(&self, workspace_path: &str, checkpoint_id: &str) -> Result<(), RpcError> {
        let checkpoint = self.load(workspace_path, checkpoint_id).await?;
        let resolution = self.ensure_repository(workspace_path).await?;
        self.git()
            .run(
                &resolution.repo_root,
                &["update-ref", "-d", &checkpoint.ref_name],
                Limits::COMMAND,
            )
            .await?
            .ensure_codes("git update-ref -d checkpoint", &[0, 1])?;
        self.inner
            .store
            .delete(workspace_path, checkpoint_id)
            .await
            .map_err(io_error)
    }
}

fn conflict(path: &str, repo_relative: &str, workspace_in_repo: &str, reason: &str) -> Value {
    json!({
        "path": path,
        "repoRelativePath": repo_relative,
        "workspaceRelativePath": to_workspace_relative(repo_relative, workspace_in_repo),
        "reason": reason,
    })
}

impl Checkpoints {
    /// 受影响路径的当前磁盘状态是否仍等于 `from`：比较 blob hash，不看时间戳和大小。
    async fn collect_conflicts(
        &self,
        resolution: &Resolution,
        from: &CheckpointMeta,
        affected: &[String],
    ) -> Result<Vec<Value>, RpcError> {
        if affected.is_empty() {
            return Ok(Vec::new());
        }
        let repo_root = resolution.repo_root.as_str();
        let workspace_in_repo = resolution.workspace_in_repo_path.as_str();
        let mut args = vec!["ls-tree", "-r", "-z", from.commit_oid.as_str(), "--"];
        args.extend(affected.iter().map(String::as_str));
        let tree = self.git().run(repo_root, &args, Limits::COMMAND).await?;
        tree.ensure("git ls-tree checkpoint paths")?;
        let tree = parse_ls_tree(&tree.stdout);

        let mut conflicts: Vec<(String, Value)> = Vec::new();
        for repo_relative in affected {
            let absolute = to_absolute(repo_root, repo_relative);
            let mut push = |reason: &str| {
                conflicts.push((
                    repo_relative.clone(),
                    conflict(&absolute, repo_relative, workspace_in_repo, reason),
                ));
            };
            let Some(expected) = tree.get(repo_relative) else {
                // from 里没有该路径，现在却存在：checkpoint 之后有人新增了它。
                if tokio::fs::symlink_metadata(&absolute).await.is_ok() {
                    push("unexpected-file-in-worktree");
                }
                continue;
            };
            let Ok(meta) = tokio::fs::symlink_metadata(&absolute).await else {
                push("missing-in-worktree");
                continue;
            };
            let expects_symlink = expected.mode == "120000";
            if meta.is_dir() || (expects_symlink && !meta.file_type().is_symlink()) {
                push("type-mismatch");
                continue;
            }
            let hash = self
                .git()
                .run(
                    repo_root,
                    &["hash-object", "--no-filters", &absolute],
                    Limits::COMMAND,
                )
                .await?;
            hash.ensure("git hash-object checkpoint verify")?;
            if hash.stdout.trim() != expected.object_id {
                push("content-mismatch");
            }
        }
        // 与 Node 的 Map 去重一致：同一路径保留首次出现的位置、最后一次的值。
        let mut order: Vec<String> = Vec::new();
        let mut by_path: HashMap<String, Value> = HashMap::new();
        for (key, value) in conflicts {
            if by_path.insert(key.clone(), value).is_none() {
                order.push(key);
            }
        }
        Ok(order
            .into_iter()
            .filter_map(|key| by_path.remove(&key))
            .collect())
    }

    /// 三方比较：from→to 的受影响路径必须仍等于 from（`force` 例外），再写回 to 并校验。
    pub async fn restore(&self, params: &PairParams) -> Result<Value, RpcError> {
        let workspace_path = params.workspace_path.as_str();
        let (from, to) = self
            .load_pair(
                workspace_path,
                &params.from_checkpoint_id,
                &params.to_checkpoint_id,
            )
            .await?;
        let resolution = self.ensure_repository(workspace_path).await?;
        let (_, diff) = self.compute_diff(workspace_path, &from, &to).await?;
        let files = diff_files(&diff);
        let repo_root = resolution.repo_root.as_str();

        // diff 里既有当前路径，也有 rename 前的旧路径（绝对路径），统一折叠成 repo 相对路径集。
        let mut affected: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for file in &files {
            for raw in std::iter::once(&file.repo_relative_path).chain(file.original_path.as_ref())
            {
                if seen.insert(raw.clone()) {
                    affected.push(normalize_affected_repo_path(repo_root, raw));
                }
            }
        }
        if affected.is_empty() {
            return Ok(json!({"success": true, "restoredPaths": []}));
        }

        let conflicts = self
            .collect_conflicts(&resolution, &from, &affected)
            .await?;
        if !conflicts.is_empty() && params.force != Some(true) {
            return Ok(json!({"success": false, "conflicts": conflicts}));
        }

        let restore: Vec<&str> = files
            .iter()
            .filter(|file| file.kind != "deleted")
            .map(|file| file.repo_relative_path.as_str())
            .collect();
        if !restore.is_empty() {
            // 只改 worktree，不改用户的真实 index。
            let source = format!("--source={}", to.commit_oid);
            let mut args = vec!["restore", source.as_str(), "--worktree", "--"];
            args.extend(restore);
            self.git()
                .run(repo_root, &args, Limits::COMMAND)
                .await?
                .ensure("git restore checkpoint")?;
        }

        let mut removed = std::collections::HashSet::new();
        for file in &files {
            let target = match (file.kind, &file.original_path) {
                ("deleted", _) => Some(&file.path),
                ("renamed", Some(original)) => Some(original),
                _ => None,
            };
            if let Some(target) = target
                && removed.insert(target.clone())
            {
                remove_if_exists(Path::new(target)).await.map_err(|error| {
                    // Node `rm` 的错误文案（Windows 上文件被占用时为 EBUSY）。
                    git_error(crate::support::node_error::fs_message(&error, "rm", target))
                })?;
            }
        }

        let verify = self.collect_conflicts(&resolution, &to, &affected).await?;
        if !verify.is_empty() {
            return Err(git_error("Checkpoint restore verification failed."));
        }
        let restored: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        Ok(json!({"success": true, "restoredPaths": restored}))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryParams {
    workspace_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointParams {
    workspace_path: String,
    checkpoint_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairParams {
    workspace_path: String,
    from_checkpoint_id: String,
    to_checkpoint_id: String,
    force: Option<bool>,
}

pub fn service(checkpoints: Checkpoints) -> ServiceBuilder {
    let (create, diff, restore, delete) = (
        checkpoints.clone(),
        checkpoints.clone(),
        checkpoints.clone(),
        checkpoints,
    );
    ServiceBuilder::new("git-checkpoint")
        .method("createCheckpoint", move |_, args| {
            let checkpoints = create.clone();
            async move {
                let params: RepositoryParams = args.get(0, "params")?;
                Ok(Reply::Value(
                    checkpoints.create(&params.workspace_path).await?,
                ))
            }
        })
        .method("diffCheckpoints", move |_, args| {
            let checkpoints = diff.clone();
            async move {
                let params: PairParams = args.get(0, "params")?;
                Ok(Reply::Value(checkpoints.diff(&params).await?))
            }
        })
        .method("restoreBetweenCheckpoints", move |_, args| {
            let checkpoints = restore.clone();
            async move {
                let params: PairParams = args.get(0, "params")?;
                Ok(Reply::Value(checkpoints.restore(&params).await?))
            }
        })
        .method("deleteCheckpoint", move |_, args| {
            let checkpoints = delete.clone();
            async move {
                let params: CheckpointParams = args.get(0, "params")?;
                checkpoints
                    .delete(&params.workspace_path, &params.checkpoint_id)
                    .await?;
                Ok(Reply::Undefined)
            }
        })
}
