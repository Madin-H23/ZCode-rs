//! git 仓库操作（packages/services/src/git/repo/gitCliRepo.ts、gitCliTypes.ts）。
//!
//! 不跨调用缓存；同一 workspacePath 的仓库解析、仓库信息与状态快照只做“进行中请求复用”，
//! 变更操作后 [`GitRepo::invalidate`] 丢掉进行中的条目。“不是仓库 / 没有 git”对读接口是数据
//! 状态（`isRepository: false`），对变更接口是错误。
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::{BoxFuture, FutureExt, Shared, join_all, try_join_all};
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use zcode_async::Spawner;
use zcode_host_kit::{RpcError, log};

use super::command::{GitOutput, GitRunner, Limits, git_error};
use super::parse::{
    self, BINARY_SUMMARY, ChangeKind, DiffResult, HeadRefType, LineStat, NumStats, StatusEntry,
    TRUNCATED_SUMMARY, normalize_git_path,
};
use crate::support::env::ProcessEnv;
use crate::support::path;

const UNTRACKED_STAT_MAX_BYTES: u64 = 1024 * 1024;
const UNTRACKED_STAT_CHUNK_BYTES: u64 = 64 * 1024;
const UNTRACKED_STAT_CONCURRENCY: usize = 4;
/// 工作区文件全文预览与未跟踪文件 patch 的上限（DEFAULT_GIT_DIFF_BYTES）。
const PREVIEW_MAX_BYTES: u64 = 1024 * 1024;
const OPERATION_MARKERS: [&str; 7] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "REBASE_HEAD",
    "rebase-merge",
    "rebase-apply",
    "BISECT_LOG",
];
const GRAPH_DEFAULT_COUNT: i64 = 100;
const GRAPH_MAX_COUNT: i64 = 200;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WatchPath {
    pub path: String,
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub workspace_path: String,
    pub repo_root: String,
    pub workspace_in_repo_path: String,
    pub auto_refresh_watch_paths: Vec<WatchPath>,
    pub is_git_available: bool,
    pub is_repository: bool,
}

impl Resolution {
    fn unavailable(workspace_path: &str, is_git_available: bool) -> Self {
        Self {
            workspace_path: workspace_path.to_owned(),
            repo_root: workspace_path.to_owned(),
            workspace_in_repo_path: ".".to_owned(),
            auto_refresh_watch_paths: Vec::new(),
            is_git_available,
            is_repository: false,
        }
    }

    pub fn is_usable(&self) -> bool {
        self.is_git_available && self.is_repository
    }

    /// `ensureRepositoryAvailable`。
    fn ensure_usable(&self, label: &str) -> Result<(), RpcError> {
        if !self.is_git_available {
            return Err(git_error(format!(
                "Cannot {label}: Git binary is not available"
            )));
        }
        if !self.is_repository {
            return Err(git_error(format!(
                "Cannot {label}: workspace is not inside a Git repository"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub workspace_path: String,
    pub repo_root: String,
    pub workspace_in_repo_path: String,
    pub auto_refresh_watch_paths: Vec<WatchPath>,
    pub branch_name: Option<String>,
    pub tracking_branch_name: Option<String>,
    pub head_ref_type: HeadRefType,
    pub ahead: u64,
    pub behind: u64,
    pub is_dirty: bool,
    pub is_git_available: bool,
    pub is_repository: bool,
}

impl Summary {
    /// `createEmptySummary`。
    fn empty(resolution: &Resolution) -> Self {
        Self {
            workspace_path: resolution.workspace_path.clone(),
            repo_root: resolution.repo_root.clone(),
            workspace_in_repo_path: resolution.workspace_in_repo_path.clone(),
            auto_refresh_watch_paths: resolution.auto_refresh_watch_paths.clone(),
            branch_name: None,
            tracking_branch_name: None,
            head_ref_type: HeadRefType::Branch,
            ahead: 0,
            behind: 0,
            is_dirty: false,
            is_git_available: resolution.is_git_available,
            is_repository: resolution.is_repository,
        }
    }
}

pub struct StatusSnapshot {
    pub resolution: Arc<Resolution>,
    pub summary: Summary,
    pub entries: Vec<StatusEntry>,
    pub staged: NumStats,
    pub unstaged: NumStats,
    pub untracked: HashMap<String, LineStat>,
}

pub struct BranchChange {
    pub path: String,
    pub original_path: Option<String>,
    pub kind: ChangeKind,
    pub added: u64,
    pub removed: u64,
}

pub struct BranchComparison {
    pub resolution: Arc<Resolution>,
    pub base_ref: Option<String>,
    pub head_ref: String,
    pub comparison_label: Option<String>,
    pub changes: Vec<BranchChange>,
}

#[derive(Clone, Copy)]
pub enum BranchAction<'a> {
    Switch,
    Create { start_point: Option<&'a str> },
}

type SharedResult<T> = Shared<BoxFuture<'static, Result<Arc<T>, RpcError>>>;
type Requests<T> = Arc<Mutex<HashMap<String, (u64, SharedResult<T>)>>>;

/// 按 key 复用进行中的请求（`reuseInFlightRequest`）；完成后自行移除表项，不缓存结果。
struct InFlight<T> {
    /// git 的作用域：请求由任务 `git-request` 跑完，调用方被取消也不打断。
    spawner: Spawner,
    requests: Requests<T>,
    next: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<T: Send + Sync + 'static> InFlight<T> {
    fn new(spawner: &Spawner) -> Self {
        Self {
            spawner: spawner.clone(),
            requests: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(0),
        }
    }

    async fn run(
        &self,
        key: &str,
        work: impl FnOnce() -> BoxFuture<'static, Result<T, RpcError>>,
    ) -> Result<Arc<T>, RpcError> {
        let shared = {
            let mut requests = lock(&self.requests);
            match requests.get(key) {
                Some((_, existing)) => existing.clone(),
                None => {
                    let token = self.next.fetch_add(1, Ordering::Relaxed);
                    let (table, owned_key, work) = (self.requests.clone(), key.to_owned(), work());
                    let shared = async move {
                        // panic 也收敛成错误并清理表项，否则这个 key 之后一直拿到已 panic 的 Shared。
                        let result = std::panic::AssertUnwindSafe(work)
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| Err(git_error("git request panicked")))
                            .map(Arc::new);
                        let mut requests = lock(&table);
                        if requests.get(&owned_key).is_some_and(|(id, _)| *id == token) {
                            requests.remove(&owned_key);
                        }
                        result
                    }
                    .boxed()
                    .shared();
                    requests.insert(key.to_owned(), (token, shared.clone()));
                    // 调用方被取消时请求也要跑完并清理表项，否则后来者会一直拿到这次的结果。
                    drop(self.spawner.spawn("git-request", shared.clone()));
                    shared
                }
            }
        };
        shared.await
    }

    fn invalidate(&self, key: &str) {
        lock(&self.requests).remove(key);
    }
}

#[derive(Clone)]
pub struct GitRepo {
    inner: Arc<Inner>,
}

struct Inner {
    git: GitRunner,
    /// `os.tmpdir()`：staged-only 提交的临时 index 放在这里。
    temp_root: String,
    resolutions: InFlight<Resolution>,
    workspace_infos: InFlight<Value>,
    statuses: InFlight<StatusSnapshot>,
    /// 详细未跟踪状态曾超出输出上限的仓库，此后一直用目录折叠模式。
    collapsed_untracked: Mutex<HashSet<String>>,
}

fn temp_root(env: &ProcessEnv) -> String {
    if cfg!(windows) {
        let fallback = format!(
            r"{}\temp",
            env.get("SystemRoot")
                .or_else(|| env.get("windir"))
                .unwrap_or(r"C:\Windows")
        );
        let dir = env
            .non_empty("TEMP")
            .or_else(|| env.non_empty("TMP"))
            .map(str::to_owned)
            .unwrap_or(fallback);
        return if dir.len() > 1 && dir.ends_with('\\') && !dir.ends_with(":\\") {
            dir[..dir.len() - 1].to_owned()
        } else {
            dir
        };
    }
    let dir = env
        .non_empty("TMPDIR")
        .or_else(|| env.non_empty("TMP"))
        .or_else(|| env.non_empty("TEMP"))
        .unwrap_or("/tmp");
    if dir.len() > 1 {
        dir.trim_end_matches('/').to_owned()
    } else {
        dir.to_owned()
    }
}

/// `normalizeWatchPath`：去掉结尾分隔符，根目录（`/`、`C:\`）保持原样。
fn normalize_watch_path(path: &str) -> String {
    let trimmed = path.trim();
    let bytes = trimmed.as_bytes();
    let drive_root = (bytes.len() == 2 || bytes.len() == 3)
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes.get(2).is_none_or(|c| matches!(c, b'\\' | b'/'));
    if trimmed == "/" || drive_root {
        return trimmed.to_owned();
    }
    trimmed.trim_end_matches(['\\', '/']).to_owned()
}

/// git 元数据目录（linked worktree 的 gitdir 与公共 gitdir）作为 UI 自动刷新的监听边界。
fn auto_refresh_watch_paths(
    workspace_path: &str,
    git_dir: &str,
    common_dir: &str,
) -> Vec<WatchPath> {
    // `--git-common-dir` 的相对结果以命令 cwd（workspacePath）为基准，不是 repoRoot。
    let common = if common_dir.is_empty() {
        git_dir.to_owned()
    } else if path::is_absolute(common_dir) {
        common_dir.to_owned()
    } else {
        path::resolve(workspace_path, &[common_dir])
    };
    let mut paths: Vec<WatchPath> = Vec::new();
    for candidate in [git_dir, common.as_str()] {
        let normalized = normalize_watch_path(candidate);
        if !normalized.is_empty() && paths.iter().all(|entry| entry.path != normalized) {
            paths.push(WatchPath {
                path: normalized,
                recursive: true,
            });
        }
    }
    paths
}

fn is_not_repository(output: &GitOutput) -> bool {
    let stderr = output.stderr.to_lowercase();
    stderr.contains("not a git repository") || stderr.contains("outside repository")
}

fn is_missing_working_directory(output: &GitOutput) -> bool {
    let stderr = output.stderr.to_lowercase();
    (output.exit_code == Some(-2) && stderr.contains("enoent"))
        || stderr.contains("unable to read current working directory")
        || stderr.contains("no such file or directory")
}

/// 输入路径的“原始绝对路径”：相对路径按 workspacePath 解析，不跟随符号链接。
fn raw_absolute(workspace_path: &str, input: &str) -> String {
    if path::is_absolute(input) {
        input.to_owned()
    } else {
        path::resolve(workspace_path, &[input])
    }
}

/// `normalizeInputPath`：realpath 后相对 repoRoot，越出仓库（含仓库根本身）报错。
async fn normalize_input_path(resolution: &Resolution, input: &str) -> Result<String, RpcError> {
    let raw = raw_absolute(&resolution.workspace_path, input);
    let absolute = path::realpath(&raw).await.unwrap_or(raw);
    let relative = normalize_git_path(&path::relative(&resolution.repo_root, &absolute));
    if relative.is_empty() || relative == "." || relative == ".." || relative.starts_with("../") {
        return Err(git_error(format!(
            "Path is outside repository scope: {input}"
        )));
    }
    Ok(relative)
}

async fn normalize_unique(
    resolution: &Resolution,
    inputs: &[String],
) -> Result<Vec<String>, RpcError> {
    let normalized = try_join_all(
        inputs
            .iter()
            .map(|input| normalize_input_path(resolution, input)),
    )
    .await?;
    let mut seen = HashSet::new();
    Ok(normalized
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect())
}

async fn exists(path: &str) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

/// 未跟踪文件行数：≤1 MiB 的普通文本文件按换行计数（末行无换行也算一行），其余为 0。
async fn count_untracked_lines(path: &str) -> std::io::Result<u64> {
    let metadata = tokio::fs::metadata(path).await?;
    if !metadata.is_file() || metadata.len() > UNTRACKED_STAT_MAX_BYTES {
        return Ok(0);
    }
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0u8; UNTRACKED_STAT_CHUNK_BYTES as usize];
    let (mut total, mut newlines, mut last) = (0u64, 0u64, b'\n');
    loop {
        // 文件可能在 stat 后增长：读取同样受预算约束，多读一字节只用于识别越界。
        let budget =
            (UNTRACKED_STAT_MAX_BYTES + 1 - total).min(UNTRACKED_STAT_CHUNK_BYTES) as usize;
        let read = file.read(&mut buffer[..budget]).await?;
        if read == 0 {
            return Ok(newlines + u64::from(last != b'\n'));
        }
        total += read as u64;
        if total > UNTRACKED_STAT_MAX_BYTES {
            return Ok(0);
        }
        for &byte in &buffer[..read] {
            match byte {
                0 => return Ok(0),
                b'\n' => newlines += 1,
                _ => {}
            }
        }
        last = buffer[read - 1];
    }
}

async fn untracked_stats(repo_root: &str, entries: &[StatusEntry]) -> HashMap<String, LineStat> {
    // 固定 4 个并发、逐文件限量读取：一次性读全部未跟踪文件会让 Host 瞬间分配大量内存。
    let targets: Vec<(String, String)> = entries
        .iter()
        .filter(|entry| entry.is_untracked)
        .map(|entry| {
            let segments: Vec<&str> = entry.path.split('/').collect();
            (entry.path.clone(), path::resolve(repo_root, &segments))
        })
        .collect();
    stream::iter(targets)
        .map(|(relative, absolute)| async move {
            let added = count_untracked_lines(&absolute).await.unwrap_or(0);
            (
                relative,
                LineStat {
                    added,
                    ..LineStat::default()
                },
            )
        })
        .buffer_unordered(UNTRACKED_STAT_CONCURRENCY)
        .collect()
        .await
}

/// 工作区一侧的全文：≤1 MiB 的普通文件且不含 NUL；读不到时返回 None（不能当成空文件）。
async fn read_worktree_preview(path: &str) -> Option<String> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > PREVIEW_MAX_BYTES {
        return None;
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (!text.contains('\0')).then_some(text)
}

/// 文件是否含 NUL；超过上限的文件流式扫描，不整读进内存。
async fn contains_nul(path: &str) -> std::io::Result<bool> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok(false);
        }
        if buffer[..read].contains(&0) {
            return Ok(true);
        }
    }
}

/// `buildUntrackedTextDiffResult`：未跟踪文本文件生成稳定的单文件 unified diff。
/// 判定顺序与 TS 相同：先看是否含 NUL（二进制），再看大小；读失败返回 None。
async fn untracked_text_diff(absolute: &str, relative: &str) -> Option<DiffResult> {
    let size = tokio::fs::metadata(absolute).await.ok()?.len();
    if size > PREVIEW_MAX_BYTES {
        let binary = contains_nul(absolute).await.ok()?;
        return Some(if binary {
            DiffResult::without_patch(absolute, "binary", BINARY_SUMMARY)
        } else {
            DiffResult::without_patch(absolute, "truncated", TRUNCATED_SUMMARY)
        });
    }
    let bytes = tokio::fs::read(absolute).await.ok()?;
    if bytes.contains(&0) {
        return Some(DiffResult::without_patch(
            absolute,
            "binary",
            BINARY_SUMMARY,
        ));
    }
    if bytes.len() as u64 > PREVIEW_MAX_BYTES {
        return Some(DiffResult::without_patch(
            absolute,
            "truncated",
            TRUNCATED_SUMMARY,
        ));
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let normalized = text.replace("\r\n", "\n");
    let mut patch = vec![
        "--- /dev/null".to_owned(),
        format!("+++ b/{}", normalize_git_path(relative)),
    ];
    if !normalized.is_empty() {
        let trailing_newline = normalized.ends_with('\n');
        let body = if trailing_newline {
            &normalized[..normalized.len() - 1]
        } else {
            normalized.as_str()
        };
        let lines: Vec<&str> = body.split('\n').collect();
        patch.push(format!("@@ -0,0 +1,{} @@", lines.len()));
        patch.extend(lines.iter().map(|line| format!("+{line}")));
        if !trailing_newline {
            patch.push("\\ No newline at end of file".to_owned());
        }
    }
    Some(DiffResult {
        path: absolute.to_owned(),
        availability: "patch",
        patch: Some(format!("{}\n", patch.join("\n"))),
        before_content: Some(String::new()),
        after_content: Some(text),
        summary: None,
    })
}

fn parse_tracking_remote(tracking: Option<&str>) -> Option<String> {
    let remote = tracking?.split('/').next()?.trim();
    (!remote.is_empty()).then(|| remote.to_owned())
}

impl GitRepo {
    /// git 的任务（复用请求的驱动、Windows 上结束进程树的 taskkill 回收）在 `spawner` 的作用域里。
    pub fn new(spawner: &Spawner, env: &ProcessEnv) -> Self {
        Self::with_runner(spawner, GitRunner::new(spawner, env), temp_root(env))
    }

    fn with_runner(spawner: &Spawner, git: GitRunner, temp_root: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                git,
                temp_root,
                resolutions: InFlight::new(spawner),
                workspace_infos: InFlight::new(spawner),
                statuses: InFlight::new(spawner),
                collapsed_untracked: Mutex::new(HashSet::new()),
            }),
        }
    }

    fn git(&self) -> &GitRunner {
        &self.inner.git
    }

    /// checkpoint 与提交信息生成复用同一个 git 执行器（同一份清洗后的环境与二进制探测）。
    pub fn runner(&self) -> &GitRunner {
        &self.inner.git
    }

    pub fn invalidate(&self, workspace_path: &str) {
        self.inner.resolutions.invalidate(workspace_path);
        self.inner.workspace_infos.invalidate(workspace_path);
        self.inner.statuses.invalidate(workspace_path);
    }

    pub async fn resolve(&self, workspace_path: &str) -> Result<Arc<Resolution>, RpcError> {
        let (repo, key) = (self.clone(), workspace_path.to_owned());
        self.inner
            .resolutions
            .run(workspace_path, move || {
                async move { repo.resolve_now(&key).await }.boxed()
            })
            .await
    }

    async fn resolve_now(&self, workspace_path: &str) -> Result<Resolution, RpcError> {
        if self.git().binary().await.is_none() {
            return Ok(Resolution::unavailable(workspace_path, false));
        }
        let args = [
            "rev-parse",
            "--show-toplevel",
            "--show-prefix",
            "--absolute-git-dir",
            "--git-common-dir",
        ];
        let output = self
            .git()
            .run(workspace_path, &args, Limits::COMMAND)
            .await?;
        if !output.succeeded() {
            // workspace 被并发删除或不在仓库里都是“不可用的仓库”，不是协议失败。
            if is_missing_working_directory(&output) || is_not_repository(&output) {
                return Ok(Resolution::unavailable(workspace_path, true));
            }
            output.ensure("git rev-parse")?;
        }
        let stdout = output.stdout.replace("\r\n", "\n");
        let lines: Vec<&str> = stdout.split('\n').collect();
        let line = |index: usize| lines.get(index).copied().unwrap_or_default();
        let repo_root = line(0).trim();
        if repo_root.is_empty() {
            return Err(git_error("Failed to resolve Git repository root"));
        }
        Ok(Resolution {
            workspace_path: workspace_path.to_owned(),
            repo_root: repo_root.to_owned(),
            workspace_in_repo_path: parse::normalize_workspace_in_repo_path(line(1)),
            auto_refresh_watch_paths: auto_refresh_watch_paths(
                workspace_path,
                line(2).trim(),
                line(3).trim(),
            ),
            is_git_available: true,
            is_repository: true,
        })
    }

    pub async fn workspace_info(&self, workspace_path: &str) -> Result<Arc<Value>, RpcError> {
        let (repo, key) = (self.clone(), workspace_path.to_owned());
        self.inner
            .workspace_infos
            .run(workspace_path, move || {
                async move { repo.workspace_info_now(&key).await }.boxed()
            })
            .await
    }

    async fn workspace_info_now(&self, workspace_path: &str) -> Result<Value, RpcError> {
        let resolution = self.resolve(workspace_path).await?;
        if !resolution.is_usable() {
            return Ok(json!({
                "workspacePath": workspace_path,
                "kind": "not-repository",
                "isGitAvailable": resolution.is_git_available,
            }));
        }
        let kind = linked_worktree_kind(&resolution.repo_root).await;
        Ok(json!({"workspacePath": workspace_path, "kind": kind, "isGitAvailable": true}))
    }

    pub async fn status(&self, workspace_path: &str) -> Result<Arc<StatusSnapshot>, RpcError> {
        let (repo, key) = (self.clone(), workspace_path.to_owned());
        self.inner
            .statuses
            .run(workspace_path, move || {
                async move { repo.status_now(&key).await }.boxed()
            })
            .await
    }

    async fn run_status(&self, resolution: &Resolution) -> Result<GitOutput, RpcError> {
        let collapsed = lock(&self.inner.collapsed_untracked).contains(&resolution.repo_root);
        let run = |mode: &'static str| {
            let flag = format!("--untracked-files={mode}");
            async move {
                self.git()
                    .run(
                        &resolution.repo_root,
                        &["status", "--porcelain=v2", "--branch", &flag, "-z"],
                        Limits::COMMAND,
                    )
                    .await
            }
        };
        let output = run(if collapsed { "normal" } else { "all" }).await?;
        if collapsed || !output.output_truncated {
            return Ok(output);
        }
        // 逐文件的未跟踪状态超限：此后这个仓库改用目录折叠模式，避免每次刷新都先失败一次。
        lock(&self.inner.collapsed_untracked).insert(resolution.repo_root.clone());
        log::warn(format!(
            "git status detailed output exceeded limit; collapsing untracked directories repoRoot={}",
            resolution.repo_root
        ));
        run("normal").await
    }

    async fn status_now(&self, workspace_path: &str) -> Result<StatusSnapshot, RpcError> {
        let resolution = self.resolve(workspace_path).await?;
        if !resolution.is_usable() {
            return Ok(StatusSnapshot {
                summary: Summary::empty(&resolution),
                resolution,
                entries: Vec::new(),
                staged: NumStats::default(),
                unstaged: NumStats::default(),
                untracked: HashMap::new(),
            });
        }
        let root = resolution.repo_root.as_str();
        let (status, staged, unstaged) = tokio::join!(
            self.run_status(&resolution),
            self.git().run(
                root,
                &[
                    "diff",
                    "--cached",
                    "--numstat",
                    "-z",
                    "--find-renames",
                    "--"
                ],
                Limits::COMMAND
            ),
            self.git().run(
                root,
                &["diff", "--numstat", "-z", "--find-renames", "--"],
                Limits::COMMAND
            ),
        );
        let (status, staged, unstaged) = (status?, staged?, unstaged?);
        status.ensure("git status")?;
        staged.ensure("git diff --cached --numstat")?;
        unstaged.ensure("git diff --numstat")?;
        let parsed = parse::parse_status_porcelain(&status.stdout);
        let untracked = untracked_stats(root, &parsed.entries).await;
        Ok(StatusSnapshot {
            summary: Summary {
                workspace_path: resolution.workspace_path.clone(),
                repo_root: resolution.repo_root.clone(),
                workspace_in_repo_path: resolution.workspace_in_repo_path.clone(),
                auto_refresh_watch_paths: resolution.auto_refresh_watch_paths.clone(),
                branch_name: parsed.branch_name,
                tracking_branch_name: parsed.tracking_branch_name,
                head_ref_type: parsed.head_ref_type,
                ahead: parsed.ahead,
                behind: parsed.behind,
                is_dirty: !parsed.entries.is_empty(),
                is_git_available: true,
                is_repository: true,
            },
            entries: parsed.entries,
            staged: parse::parse_numstat(&staged.stdout),
            unstaged: parse::parse_numstat(&unstaged.stdout),
            untracked,
            resolution,
        })
    }

    /// `getIgnoredPaths`：返回被忽略输入的原始绝对路径（不 realpath），保持输入顺序。
    pub async fn ignored_paths(
        &self,
        workspace_path: &str,
        inputs: &[String],
    ) -> Result<Vec<String>, RpcError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let resolution = self.resolve(workspace_path).await?;
        if !resolution.is_usable() {
            return Ok(Vec::new());
        }
        let pairs: Vec<(String, String)> = join_all(inputs.iter().map(|input| {
            let resolution = &resolution;
            async move {
                let relative = normalize_input_path(resolution, input).await.ok()?;
                Some((raw_absolute(&resolution.workspace_path, input), relative))
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect();
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["check-ignore", "--"];
        args.extend(pairs.iter().map(|(_, relative)| relative.as_str()));
        let output = self
            .git()
            .run(&resolution.repo_root, &args, Limits::COMMAND)
            .await?;
        if output.exit_code == Some(1) {
            return Ok(Vec::new());
        }
        output.ensure("git check-ignore")?;
        // `-z` 只能配合 `--stdin`；这里用 argv 传路径，所以按普通换行解析。
        let ignored: HashSet<String> = output
            .stdout
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .filter(|line| !line.is_empty())
            .map(normalize_git_path)
            .collect();
        Ok(pairs
            .into_iter()
            .filter(|(_, relative)| ignored.contains(relative))
            .map(|(absolute, _)| absolute)
            .collect())
    }

    pub async fn local_branches(&self, workspace_path: &str) -> Result<Value, RpcError> {
        let status = self.status(workspace_path).await?;
        let summary = &status.summary;
        if !status.resolution.is_usable() {
            return Ok(json!({
                "headRefType": summary.head_ref_type,
                "currentBranchName": summary.branch_name,
                "branches": [],
            }));
        }
        let output = self
            .git()
            .run(
                &status.resolution.repo_root,
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--format=%(refname:short)%00%(upstream:short)%00%(objectname)%00%(committerdate:unix)",
                ],
                Limits::COMMAND,
            )
            .await?;
        output.ensure("git for-each-ref refs/heads")?;
        let current = match summary.head_ref_type {
            HeadRefType::Branch => summary.branch_name.as_deref(),
            HeadRefType::Detached => None,
        };
        Ok(json!({
            "headRefType": summary.head_ref_type,
            "currentBranchName": summary.branch_name,
            "branches": parse::parse_branch_refs(&output.stdout, current),
        }))
    }

    pub async fn commit_graph(
        &self,
        workspace_path: &str,
        max_count: Option<f64>,
        skip: Option<f64>,
    ) -> Result<Value, RpcError> {
        let resolution = self.resolve(workspace_path).await?;
        if !resolution.is_usable() {
            return Ok(json!({"commits": [], "hasMore": false}));
        }
        let count = match max_count {
            Some(count) if count.is_finite() => (count.floor() as i64).clamp(1, GRAPH_MAX_COUNT),
            _ => GRAPH_DEFAULT_COUNT,
        };
        let skip = match skip {
            Some(skip) if skip.is_finite() => (skip.floor() as i64).max(0),
            _ => 0,
        };
        let (skip_arg, count_arg) = (
            format!("--skip={skip}"),
            format!("--max-count={}", count + 1),
        );
        // 不用 --all：refs/zcode/checkpoints 等内部引用不能进 Graph。
        let output = self
            .git()
            .run(
                &resolution.repo_root,
                &[
                    "log",
                    "HEAD",
                    "--branches",
                    "--tags",
                    "--remotes",
                    "--date-order",
                    "--topo-order",
                    &skip_arg,
                    &count_arg,
                    "--format=%H%x00%P%x00%an%x00%at%x00%s%x00%D%x1e",
                ],
                Limits::COMMAND,
            )
            .await?;
        if !output.succeeded() {
            let stderr = output.stderr.to_lowercase();
            if [
                "does not have any commits yet",
                "your current branch",
                "bad default revision",
                "ambiguous argument 'head'",
            ]
            .iter()
            .any(|marker| stderr.contains(marker))
            {
                return Ok(json!({"commits": [], "hasMore": false}));
            }
            output.ensure("git log visible refs")?;
        }
        let mut commits = parse::parse_graph_records(&output.stdout);
        let has_more = commits.len() as i64 > count;
        commits.truncate(count as usize);
        Ok(json!({"commits": commits, "hasMore": has_more}))
    }

    async fn operation_in_progress(&self, repo_root: &str) -> Result<bool, RpcError> {
        let mut args = vec!["rev-parse"];
        for marker in OPERATION_MARKERS {
            args.extend(["--git-path", marker]);
        }
        let output = self.git().run(repo_root, &args, Limits::COMMAND).await?;
        output.ensure("git rev-parse --git-path")?;
        let candidates: Vec<String> = output
            .stdout
            .replace("\r\n", "\n")
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| {
                if path::is_absolute(line) {
                    line.to_owned()
                } else {
                    path::resolve(repo_root, &[line])
                }
            })
            .collect();
        Ok(
            join_all(candidates.iter().map(|candidate| exists(candidate)))
                .await
                .into_iter()
                .any(|found| found),
        )
    }

    async fn invalid_branch_name(
        &self,
        repo_root: &str,
        name: &str,
    ) -> Result<Option<Value>, RpcError> {
        let output = self
            .git()
            .run(
                repo_root,
                &["check-ref-format", "--branch", name],
                Limits::COMMAND,
            )
            .await?;
        if output.timed_out || output.output_truncated {
            output.ensure("git check-ref-format --branch")?;
        }
        Ok((!output.succeeded()).then(|| parse::invalid_branch_name_issue(Some(&output.stderr))))
    }

    /// `switchBranch` / `createBranchAndSwitch`：先用已知状态给出稳定的阻塞原因，再交给 git。
    pub async fn mutate_branch(
        &self,
        workspace_path: &str,
        branch_name: &str,
        action: BranchAction<'_>,
    ) -> Result<Value, RpcError> {
        let (label, action_name, creates) = match action {
            BranchAction::Switch => ("switch branches", "switch", false),
            BranchAction::Create { .. } => {
                ("create and switch branches", "create-and-switch", true)
            }
        };
        let status = self.status(workspace_path).await?;
        status.resolution.ensure_usable(label)?;
        let root = status.resolution.repo_root.as_str();
        let result = |ok: bool,
                      name: Option<&str>,
                      did_change: bool,
                      summary: &Summary,
                      issues: Vec<Value>| {
            json!({
                "ok": ok,
                "action": action_name,
                "branchName": name,
                "didChange": did_change,
                "created": ok && did_change && creates,
                "summary": summary,
                "issues": issues,
            })
        };
        let name = branch_name.trim();
        if name.is_empty() {
            return Ok(result(
                false,
                None,
                false,
                &status.summary,
                vec![parse::invalid_branch_name_issue(None)],
            ));
        }
        if matches!(action, BranchAction::Switch)
            && status.summary.head_ref_type == HeadRefType::Branch
            && status.summary.branch_name.as_deref() == Some(name)
        {
            return Ok(result(true, Some(name), false, &status.summary, Vec::new()));
        }
        let blocked = |code: &str, message: &str| {
            Ok(result(
                false,
                Some(name),
                false,
                &status.summary,
                vec![json!({"code": code, "message": message})],
            ))
        };
        if status.entries.iter().any(|entry| entry.is_conflicted) {
            return blocked(
                "conflicts-present",
                "Repository still has unresolved conflicts.",
            );
        }
        if self.operation_in_progress(root).await? {
            return blocked(
                "operation-in-progress",
                "Another Git operation is still in progress.",
            );
        }
        if let Some(issue) = self.invalid_branch_name(root, name).await? {
            return Ok(result(
                false,
                Some(name),
                false,
                &status.summary,
                vec![issue],
            ));
        }
        let mut args = vec!["switch", "--no-guess"];
        if let BranchAction::Create { start_point } = action {
            args.extend(["-c", name]);
            // startPoint 以 `-` 开头时会被当成选项，`--` 终止选项解析。
            if let Some(start) = start_point.map(str::trim).filter(|start| !start.is_empty()) {
                args.extend(["--", start]);
            }
        } else {
            args.push(name);
        }
        let output = self.git().run(root, &args, Limits::COMMAND).await?;
        if !output.succeeded() {
            return Ok(result(
                false,
                Some(name),
                false,
                &status.summary,
                parse::parse_branch_mutation_issues(&output),
            ));
        }
        self.invalidate(workspace_path);
        let next = self.status(workspace_path).await?;
        Ok(result(true, Some(name), true, &next.summary, Vec::new()))
    }

    async fn blob(
        &self,
        repo_root: &str,
        reference: &str,
        relative: &str,
    ) -> Result<Option<String>, RpcError> {
        let spec = format!("{reference}:{relative}");
        let output = self
            .git()
            .run(repo_root, &["show", &spec], Limits::DIFF)
            .await?;
        let usable = !output.timed_out
            && !output.output_truncated
            && output.succeeded()
            && !output.stdout.contains('\0');
        Ok(usable.then_some(output.stdout))
    }

    async fn blob_pair(
        &self,
        repo_root: &str,
        relative: &str,
        before: &str,
        after: &str,
    ) -> Result<Option<(String, String)>, RpcError> {
        let (before, after) = tokio::join!(
            self.blob(repo_root, before, relative),
            self.blob(repo_root, after, relative)
        );
        Ok(before?.zip(after?))
    }

    pub async fn diff(
        &self,
        workspace_path: &str,
        input: &str,
        staged: Option<bool>,
        source_id: Option<&str>,
    ) -> Result<DiffResult, RpcError> {
        let resolution = self.resolve(workspace_path).await?;
        let absolute = raw_absolute(workspace_path, input);
        if !resolution.is_git_available {
            return Ok(DiffResult::unavailable(
                &absolute,
                "Git binary is not available in the current environment.",
            ));
        }
        if !resolution.is_repository {
            return Ok(DiffResult::unavailable(
                &absolute,
                "Workspace is not inside a Git repository.",
            ));
        }
        let relative = normalize_input_path(&resolution, input).await?;
        let root = resolution.repo_root.as_str();

        if source_id == Some("branch") {
            let status = self.status(workspace_path).await?;
            let Some(tracking) = status.summary.tracking_branch_name.clone() else {
                return Ok(DiffResult::unavailable(
                    &absolute,
                    "Current branch does not have an upstream branch.",
                ));
            };
            let range = format!("{tracking}...HEAD");
            let output = self
                .git()
                .run(
                    root,
                    &[
                        "diff",
                        "--no-ext-diff",
                        "--no-color",
                        "--binary",
                        "--find-renames",
                        &range,
                        "--",
                        &relative,
                    ],
                    Limits::DIFF,
                )
                .await?;
            let parsed = parse::to_diff_result(
                &absolute,
                &output,
                &[0],
                "No branch comparison diff is available for this file.",
            );
            if parsed.availability != "patch" {
                return Ok(parsed);
            }
            let merge_base = self
                .git()
                .run(root, &["merge-base", &tracking, "HEAD"], Limits::COMMAND)
                .await?;
            let base = if merge_base.succeeded() && !merge_base.stdout.trim().is_empty() {
                merge_base.stdout.trim().to_owned()
            } else {
                tracking
            };
            let contents = self.blob_pair(root, &relative, &base, "HEAD").await?;
            return Ok(parsed.with_contents(contents));
        }

        let staged = staged.unwrap_or(source_id == Some("staged"));
        let args: Vec<&str> = if staged {
            vec![
                "diff",
                "--cached",
                "--no-ext-diff",
                "--no-color",
                "--binary",
                "--",
                &relative,
            ]
        } else {
            vec![
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--binary",
                "--",
                &relative,
            ]
        };
        let output = self.git().run(root, &args, Limits::DIFF).await?;
        let parsed = parse::to_diff_result(
            &absolute,
            &output,
            &[0],
            "No Git diff is available for this file.",
        );
        if parsed.availability != "unavailable" || staged {
            if parsed.availability != "patch" {
                return Ok(parsed);
            }
            let contents = if staged {
                self.blob_pair(root, &relative, "HEAD", "").await?
            } else {
                let (before, after) = tokio::join!(
                    self.blob(root, "", &relative),
                    read_worktree_preview(&absolute)
                );
                before?.zip(after)
            };
            return Ok(parsed.with_contents(contents));
        }

        // 未跟踪文件不在 `git diff` 里：先生成稳定的单文件 patch，读不了再退回 --no-index。
        if !exists(&absolute).await {
            return Ok(parsed);
        }
        if let Some(result) = untracked_text_diff(&absolute, &relative).await {
            return Ok(result);
        }
        let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let output = self
            .git()
            .run(
                root,
                &[
                    "diff",
                    "--no-index",
                    "--no-ext-diff",
                    "--no-color",
                    "--binary",
                    null_device,
                    &absolute,
                ],
                Limits::DIFF,
            )
            .await?;
        Ok(parse::to_diff_result(
            &absolute,
            &output,
            &[0, 1],
            "No previewable diff is available for this file.",
        ))
    }

    pub async fn branch_comparison(
        &self,
        workspace_path: &str,
    ) -> Result<BranchComparison, RpcError> {
        let status = self.status(workspace_path).await?;
        let summary = &status.summary;
        let head_ref = summary
            .branch_name
            .clone()
            .unwrap_or_else(|| "HEAD".to_owned());
        let Some(tracking) = summary
            .tracking_branch_name
            .clone()
            .filter(|_| status.resolution.is_usable())
        else {
            return Ok(BranchComparison {
                resolution: status.resolution.clone(),
                base_ref: summary.tracking_branch_name.clone(),
                head_ref,
                comparison_label: None,
                changes: Vec::new(),
            });
        };
        let range = format!("{tracking}...HEAD");
        let output = self
            .git()
            .run(
                &status.resolution.repo_root,
                &["diff", "--numstat", "-z", "--find-renames", &range, "--"],
                Limits::COMMAND,
            )
            .await?;
        output.ensure("git diff --numstat upstream...HEAD")?;
        let changes = parse::parse_numstat(&output.stdout)
            .iter()
            .map(|(path, stat)| BranchChange {
                path: path.clone(),
                original_path: stat.original_path.clone(),
                kind: parse::kind_from_numstat(stat),
                added: stat.added,
                removed: stat.removed,
            })
            .collect();
        Ok(BranchComparison {
            resolution: status.resolution.clone(),
            comparison_label: Some(format!("{head_ref} -> {tracking}")),
            base_ref: Some(tracking),
            head_ref,
            changes,
        })
    }

    /// stage / unstage / discard：去重后的仓库相对路径交给一条命令。
    pub async fn mutate_paths(
        &self,
        workspace_path: &str,
        inputs: &[String],
        label: &str,
        command: &[&str],
        command_label: &str,
    ) -> Result<(), RpcError> {
        let resolution = self.resolve(workspace_path).await?;
        resolution.ensure_usable(label)?;
        let paths = normalize_unique(&resolution, inputs).await?;
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = command.to_vec();
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
        let output = self
            .git()
            .run(&resolution.repo_root, &args, Limits::COMMAND)
            .await?;
        output.ensure(command_label)?;
        self.invalidate(workspace_path);
        Ok(())
    }

    pub async fn commit(
        &self,
        workspace_path: &str,
        message: &str,
        paths: Option<&[String]>,
        staged_only: bool,
    ) -> Result<String, RpcError> {
        let resolution = self.resolve(workspace_path).await?;
        resolution.ensure_usable("commit changes")?;
        let message = message.trim();
        if message.is_empty() {
            return Err(git_error("Commit message cannot be empty"));
        }
        let paths = match paths {
            Some(paths) if !paths.is_empty() => normalize_unique(&resolution, paths).await?,
            _ => Vec::new(),
        };
        let root = resolution.repo_root.as_str();
        if staged_only && !paths.is_empty() {
            return self
                .commit_staged_paths(workspace_path, root, message, &paths)
                .await;
        }
        let mut args = vec!["commit", "-m", message];
        if !paths.is_empty() {
            args.push("--");
            args.extend(paths.iter().map(String::as_str));
        }
        self.git()
            .run(root, &args, Limits::COMMAND)
            .await?
            .ensure("git commit")?;
        self.invalidate(workspace_path);
        let head = self
            .git()
            .run(root, &["rev-parse", "HEAD"], Limits::COMMAND)
            .await?;
        head.ensure("git rev-parse HEAD")?;
        Ok(head.stdout.trim().to_owned())
    }

    /// 只提交所选路径的已暂存版本：在临时 index 上从 HEAD 重建这些路径并提交，其余暂存项不动。
    async fn commit_staged_paths(
        &self,
        workspace_path: &str,
        root: &str,
        message: &str,
        paths: &[String],
    ) -> Result<String, RpcError> {
        let with_paths = |prefix: &[&'static str]| {
            let mut args: Vec<&str> = prefix.to_vec();
            args.extend(paths.iter().map(String::as_str));
            args
        };
        let scoped = self
            .git()
            .run(
                root,
                &with_paths(&["status", "--porcelain=v2", "-z", "--"]),
                Limits::COMMAND,
            )
            .await?;
        scoped.ensure("git status selected paths")?;
        let mut cleanup: Vec<String> = paths.to_vec();
        for entry in parse::parse_status_porcelain(&scoped.stdout).entries {
            if let Some(original) = entry.original_path.filter(|_| paths.contains(&entry.path))
                && !cleanup.contains(&original)
            {
                cleanup.push(original);
            }
        }
        let staged = self
            .git()
            .run(
                root,
                &with_paths(&["ls-files", "--stage", "-z", "--"]),
                Limits::COMMAND,
            )
            .await?;
        staged.ensure("git ls-files selected staged entries")?;
        let entries = parse::parse_index_entries(&staged.stdout)?;
        if entries.iter().any(|entry| entry.stage != "0") {
            return Err(git_error(
                "Cannot commit selected staged paths while index conflicts exist.",
            ));
        }
        let head = self
            .git()
            .run(root, &["rev-parse", "--verify", "HEAD"], Limits::COMMAND)
            .await?;
        let parent = head.succeeded().then(|| head.stdout.trim().to_owned());

        let temp_dir = self.make_temp_dir().await?;
        let index_file = path::resolve(&temp_dir, &["index"]);
        let result = async {
            let env = [("GIT_INDEX_FILE", index_file.as_str())];
            let read_tree: Vec<&str> = match &parent {
                Some(parent) => vec!["read-tree", parent],
                None => vec!["read-tree", "--empty"],
            };
            self.git()
                .run_with_env(root, &read_tree, Limits::COMMAND, &env)
                .await?
                .ensure("git read-tree selected commit base")?;
            if !cleanup.is_empty() {
                let mut args = vec!["update-index", "--force-remove", "--"];
                args.extend(cleanup.iter().map(String::as_str));
                self.git()
                    .run_with_env(root, &args, Limits::COMMAND, &env)
                    .await?
                    .ensure("git update-index remove selected paths")?;
            }
            for entry in &entries {
                let args = [
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &entry.mode,
                    &entry.object_hash,
                    &entry.path,
                ];
                self.git()
                    .run_with_env(root, &args, Limits::COMMAND, &env)
                    .await?
                    .ensure("git update-index add selected paths")?;
            }
            self.git()
                .run_with_env(root, &["commit", "-m", message], Limits::COMMAND, &env)
                .await?
                .ensure("git commit selected staged paths")?;
            let hash = self
                .git()
                .run(root, &["rev-parse", "HEAD"], Limits::COMMAND)
                .await?;
            hash.ensure("git rev-parse selected commit HEAD")?;
            // 只把已提交的路径在真实 index 里同步到新 HEAD，其它暂存项继续等待用户提交。
            let mut reset = vec!["reset", "--quiet", "HEAD", "--"];
            reset.extend(cleanup.iter().map(String::as_str));
            self.git()
                .run(root, &reset, Limits::COMMAND)
                .await?
                .ensure("git reset selected committed paths")?;
            self.invalidate(workspace_path);
            Ok(hash.stdout.trim().to_owned())
        }
        .await;
        if let Err(error) = tokio::fs::remove_dir_all(&temp_dir).await {
            log::warn(format!(
                "failed to remove temporary git index {temp_dir}: {error}"
            ));
        }
        result
    }

    /// `mkdtemp(tmpdir()/zcode-git-index-)`。
    async fn make_temp_dir(&self) -> Result<String, RpcError> {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        for _ in 0..16 {
            let mut random = [0u8; 6];
            getrandom::fill(&mut random).map_err(|error| git_error(error.to_string()))?;
            let suffix: String = random
                .iter()
                .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
                .collect();
            let dir = path::resolve(
                &self.inner.temp_root,
                &[&format!("zcode-git-index-{suffix}")],
            );
            match tokio::fs::create_dir(&dir).await {
                Ok(()) => return Ok(dir),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(git_error(error.to_string())),
            }
        }
        Err(git_error(
            "failed to create a temporary git index directory",
        ))
    }

    async fn optional_config(
        &self,
        repo_root: &str,
        key: &str,
    ) -> Result<Option<String>, RpcError> {
        let output = self
            .git()
            .run(repo_root, &["config", "--get", key], Limits::COMMAND)
            .await?;
        let label = format!("git config --get {key}");
        match output.exit_code {
            _ if output.timed_out || output.output_truncated => {
                output.ensure(&label).map(|()| None)
            }
            Some(0) => {
                let value = output.stdout.trim();
                Ok((!value.is_empty()).then(|| value.to_owned()))
            }
            Some(1) => Ok(None),
            _ => output.ensure(&label).map(|()| None),
        }
    }

    async fn push_remote(&self, repo_root: &str, branch: &str) -> Result<String, RpcError> {
        if let Some(remote) = self
            .optional_config(repo_root, &format!("branch.{branch}.remote"))
            .await?
        {
            return Ok(remote);
        }
        if let Some(remote) = self
            .optional_config(repo_root, "remote.pushDefault")
            .await?
        {
            return Ok(remote);
        }
        let output = self
            .git()
            .run(repo_root, &["remote"], Limits::COMMAND)
            .await?;
        output.ensure("git remote")?;
        let remotes: Vec<&str> = output
            .stdout
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if remotes.contains(&"origin") {
            return Ok("origin".to_owned());
        }
        match remotes.as_slice() {
            [only] => Ok((*only).to_owned()),
            [] => Err(git_error(
                "No Git remote is configured for the current repository.",
            )),
            _ => Err(git_error(
                "Multiple Git remotes are configured. Configure branch.<name>.remote or remote.pushDefault first.",
            )),
        }
    }

    pub async fn push(&self, workspace_path: &str) -> Result<Value, RpcError> {
        let status = self.status(workspace_path).await?;
        status.resolution.ensure_usable("push changes")?;
        let root = status.resolution.repo_root.as_str();
        let branch = status
            .summary
            .branch_name
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if status.summary.head_ref_type != HeadRefType::Branch || branch.is_empty() {
            return Err(git_error("Cannot push while HEAD is detached."));
        }
        let tracking = status.summary.tracking_branch_name.as_deref();
        let (remote, args) = match tracking {
            Some(_) => (parse_tracking_remote(tracking), vec!["push".to_owned()]),
            None => {
                let remote = self.push_remote(root, &branch).await?;
                let args = vec![
                    "push".to_owned(),
                    "--set-upstream".to_owned(),
                    remote.clone(),
                    branch.clone(),
                ];
                (Some(remote), args)
            }
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.git()
            .run(root, &args, Limits::PUSH)
            .await?
            .ensure("git push")?;
        self.invalidate(workspace_path);
        let next = self.status(workspace_path).await?;
        let next_tracking = next.summary.tracking_branch_name.clone();
        let remote = remote.or_else(|| parse_tracking_remote(next_tracking.as_deref()));
        Ok(json!({
            "branchName": branch,
            "trackingBranchName": next_tracking,
            "remoteName": remote,
            "setUpstream": tracking.is_none(),
            "summary": next.summary,
        }))
    }

    pub async fn identity(&self, workspace_path: &str) -> Result<Value, RpcError> {
        let resolution = self.resolve(workspace_path).await?;
        if !resolution.is_usable() {
            return Ok(json!({
                "userName": null, "userEmail": null, "nameSource": null, "emailSource": null, "scopeLabel": null,
            }));
        }
        let root = resolution.repo_root.as_str();
        let (name, email) = tokio::join!(
            self.git().run(
                root,
                &[
                    "config",
                    "--show-scope",
                    "--show-origin",
                    "--get",
                    "user.name"
                ],
                Limits::COMMAND
            ),
            self.git().run(
                root,
                &[
                    "config",
                    "--show-scope",
                    "--show-origin",
                    "--get",
                    "user.email"
                ],
                Limits::COMMAND
            ),
        );
        let name = parse::parse_config_value(&name?)?;
        let email = parse::parse_config_value(&email?)?;
        Ok(json!({
            "userName": name.value,
            "userEmail": email.value,
            "nameSource": name.source,
            "emailSource": email.source,
            "scopeLabel": name.scope.or(email.scope),
        }))
    }
}

/// `.git` 是目录为 main-tree；是指向 `<main>/.git/worktrees/<name>` 的 gitdir 文件为 linked-worktree；
/// 其他布局或读取失败一律按 main-tree 放行（只用于迁移候选过滤）。
async fn linked_worktree_kind(repo_root: &str) -> &'static str {
    let entry = path::resolve(repo_root, &[".git"]);
    let Ok(metadata) = tokio::fs::metadata(&entry).await else {
        return "main-tree";
    };
    if !metadata.is_file() {
        return "main-tree";
    }
    let Ok(bytes) = tokio::fs::read(&entry).await else {
        return "main-tree";
    };
    let content = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    let first = content.split('\n').next().unwrap_or_default().trim();
    let Some(raw) = first.strip_prefix("gitdir:").map(str::trim) else {
        return "main-tree";
    };
    if raw.is_empty() {
        return "main-tree";
    }
    let resolved = if path::is_absolute(raw) {
        raw.to_owned()
    } else {
        path::resolve(repo_root, &[raw])
    };
    if normalize_git_path(&resolved).contains("/.git/worktrees/") {
        "linked-worktree"
    } else {
        "main-tree"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_paths_resolve_common_dir_against_the_workspace() {
        let paths = auto_refresh_watch_paths("/repo/sub", "/repo/.git", "../.git");
        assert_eq!(
            paths,
            vec![WatchPath {
                path: "/repo/.git".to_owned(),
                recursive: true
            }]
        );
        let linked = auto_refresh_watch_paths("/wt", "/main/.git/worktrees/wt/", "/main/.git");
        assert_eq!(linked.len(), 2);
        assert_eq!(linked[0].path, "/main/.git/worktrees/wt");
        assert_eq!(normalize_watch_path("C:\\"), "C:\\");
        assert_eq!(normalize_watch_path("/"), "/");
    }

    #[test]
    fn temp_root_follows_node_tmpdir() {
        let env = ProcessEnv::new([("TMPDIR".to_owned(), "/var/tmp/".to_owned())].into());
        if cfg!(windows) {
            return;
        }
        assert_eq!(temp_root(&env), "/var/tmp");
        assert_eq!(temp_root(&ProcessEnv::default()), "/tmp");
    }
}
