//! `git` channel（packages/services/src/git/gitService.ts、git.ts）。
//!
//! 运行真实的 git CLI（[`command`]），解析逻辑在 [`parse`]，仓库级流程在 [`repo`]；本文件负责
//! 按 workspace 作用域把状态快照切成 UI 用的变更列表。`generateCommitMessage` 需要回调 Agent，
//! 仍由契约 stub 应答。
pub mod command;
pub mod parse;
pub mod repo;
#[cfg(all(test, unix))]
mod tests;

use std::future::Future;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zcode_host_kit::{Args, CallContext, Reply, RpcError, ServiceBuilder};

use crate::support::path;
use parse::{
    ChangeKind, StatusEntry, is_in_workspace_scope, normalize_git_path, to_workspace_relative,
};
use repo::{BranchAction, BranchComparison, GitRepo, StatusSnapshot};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileChange {
    path: String,
    repo_relative_path: String,
    workspace_relative_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    x: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    y: Option<String>,
    kind: ChangeKind,
    section: &'static str,
    added: u64,
    removed: u64,
    is_staged: bool,
    is_untracked: bool,
    is_conflicted: bool,
}

fn absolute_in_repo(repo_root: &str, repo_relative_path: &str) -> String {
    let normalized = normalize_git_path(repo_relative_path);
    let segments: Vec<&str> = normalized.split('/').collect();
    path::resolve(repo_root, &segments)
}

fn in_scope(workspace_in_repo_path: &str, path: &str, original_path: Option<&str>) -> bool {
    is_in_workspace_scope(path, workspace_in_repo_path)
        || original_path
            .is_some_and(|original| is_in_workspace_scope(original, workspace_in_repo_path))
}

#[allow(clippy::too_many_arguments)]
fn file_change(
    repo_root: &str,
    workspace_in_repo_path: &str,
    path: &str,
    x: Option<String>,
    y: Option<String>,
    kind: ChangeKind,
    section: &'static str,
    (added, removed): (u64, u64),
) -> FileChange {
    FileChange {
        path: absolute_in_repo(repo_root, path),
        repo_relative_path: path.to_owned(),
        workspace_relative_path: to_workspace_relative(path, workspace_in_repo_path),
        x,
        y,
        kind,
        section,
        added,
        removed,
        is_staged: section == "staged",
        is_untracked: section == "untracked",
        is_conflicted: section == "conflicted",
    }
}

fn change_for_source(
    snapshot: &StatusSnapshot,
    entry: &StatusEntry,
    staged: bool,
) -> Option<FileChange> {
    let workspace = snapshot.summary.workspace_in_repo_path.as_str();
    if !in_scope(workspace, &entry.path, entry.original_path.as_deref()) {
        return None;
    }
    let build = |section, stat: (u64, u64)| {
        Some(file_change(
            &snapshot.resolution.repo_root,
            workspace,
            &entry.path,
            entry.x.clone(),
            entry.y.clone(),
            entry.kind,
            section,
            stat,
        ))
    };
    let stat_of =
        |stat: Option<&parse::LineStat>| stat.map(|s| (s.added, s.removed)).unwrap_or((0, 0));
    if staged {
        if entry.is_untracked
            || entry.is_conflicted
            || matches!(entry.x.as_deref(), None | Some("") | Some("."))
        {
            return None;
        }
        return build("staged", stat_of(snapshot.staged.get(&entry.path)));
    }
    if entry.is_conflicted {
        return build("conflicted", (0, 0));
    }
    if entry.is_untracked {
        let stat = stat_of(snapshot.untracked.get(&entry.path));
        // 空文件、二进制、超大未跟踪文件没有可审查的行；但折叠成目录（结尾 `/`）的记录必须保留。
        if !entry.path.ends_with('/') && stat == (0, 0) {
            return None;
        }
        return build("untracked", stat);
    }
    if matches!(entry.y.as_deref(), None | Some("") | Some(".")) {
        return None;
    }
    build("unstaged", stat_of(snapshot.unstaged.get(&entry.path)))
}

/// 按 workspace 作用域切出 staged 或 unstaged（含 untracked、conflicted）变更，保持 porcelain 顺序。
fn changes_for_source(snapshot: &StatusSnapshot, staged: bool) -> Vec<FileChange> {
    snapshot
        .entries
        .iter()
        .filter_map(|entry| change_for_source(snapshot, entry, staged))
        .collect()
}

fn branch_comparison_value(comparison: &BranchComparison) -> Value {
    let workspace = comparison.resolution.workspace_in_repo_path.as_str();
    let changes: Vec<FileChange> = comparison
        .changes
        .iter()
        .filter(|change| in_scope(workspace, &change.path, change.original_path.as_deref()))
        .map(|change| {
            file_change(
                &comparison.resolution.repo_root,
                workspace,
                &change.path,
                None,
                None,
                change.kind,
                "branch",
                (change.added, change.removed),
            )
        })
        .collect();
    json!({
        "baseRef": comparison.base_ref,
        "headRef": comparison.head_ref,
        "comparisonLabel": comparison.comparison_label,
        "changes": changes,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoRequest {
    workspace_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshRequest {
    workspace_path: String,
    include_identity: Option<bool>,
    include_branch_comparison: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangesRequest {
    workspace_path: String,
    source_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathsRequest {
    workspace_path: String,
    paths: Vec<String>,
    staged: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiffRequest {
    workspace_path: String,
    path: String,
    staged: Option<bool>,
    source_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphRequest {
    workspace_path: String,
    max_count: Option<f64>,
    skip: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchRequest {
    workspace_path: String,
    target_branch_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBranchRequest {
    workspace_path: String,
    branch_name: String,
    start_point: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitRequest {
    workspace_path: String,
    message: String,
    paths: Option<Vec<String>>,
    staged_only: Option<bool>,
}

type Handler =
    Box<dyn Fn(CallContext, Args) -> BoxFuture<'static, Result<Reply, RpcError>> + Send + Sync>;

/// 每个方法只收一个对象参数；解码失败按参数错误应答。
fn call<P, F, Fut>(repo: &GitRepo, handler: F) -> Handler
where
    P: DeserializeOwned + Send + 'static,
    F: Fn(GitRepo, P) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Reply, RpcError>> + Send + 'static,
{
    let repo = repo.clone();
    Box::new(move |_, args| match args.get::<P>(0, "params") {
        Ok(params) => handler(repo.clone(), params).boxed(),
        Err(error) => async move { Err(error) }.boxed(),
    })
}

fn register(builder: ServiceBuilder, name: &str, handler: Handler) -> ServiceBuilder {
    builder.method(name, handler)
}

pub fn service(repo: GitRepo) -> ServiceBuilder {
    let methods: Vec<(&str, Handler)> = vec![
        (
            "getRepositorySummary",
            call(&repo, |repo, p: RepoRequest| async move {
                Ok(Reply::of(&repo.status(&p.workspace_path).await?.summary))
            }),
        ),
        (
            "getWorkspaceRepositoryInfo",
            call(&repo, |repo, p: RepoRequest| async move {
                Ok(Reply::Value(
                    (*repo.workspace_info(&p.workspace_path).await?).clone(),
                ))
            }),
        ),
        (
            "getLocalBranches",
            call(&repo, |repo, p: RepoRequest| async move {
                Ok(Reply::Value(repo.local_branches(&p.workspace_path).await?))
            }),
        ),
        (
            "getCommitGraph",
            call(&repo, |repo, p: GraphRequest| async move {
                Ok(Reply::Value(
                    repo.commit_graph(&p.workspace_path, p.max_count, p.skip)
                        .await?,
                ))
            }),
        ),
        (
            "switchBranch",
            call(&repo, |repo, p: SwitchRequest| async move {
                let result = repo
                    .mutate_branch(
                        &p.workspace_path,
                        &p.target_branch_name,
                        BranchAction::Switch,
                    )
                    .await?;
                Ok(Reply::Value(result))
            }),
        ),
        (
            "createBranchAndSwitch",
            call(&repo, |repo, p: CreateBranchRequest| async move {
                let action = BranchAction::Create {
                    start_point: p.start_point.as_deref(),
                };
                Ok(Reply::Value(
                    repo.mutate_branch(&p.workspace_path, &p.branch_name, action)
                        .await?,
                ))
            }),
        ),
        (
            "getChanges",
            call(&repo, |repo, p: ChangesRequest| async move {
                let status = repo.status(&p.workspace_path).await?;
                Ok(Reply::of(&changes_for_source(
                    &status,
                    p.source_id == "staged",
                )))
            }),
        ),
        (
            "getIgnoredPaths",
            call(&repo, |repo, p: PathsRequest| async move {
                Ok(Reply::of(
                    &repo.ignored_paths(&p.workspace_path, &p.paths).await?,
                ))
            }),
        ),
        (
            "getDiff",
            call(&repo, |repo, p: DiffRequest| async move {
                let diff = repo
                    .diff(&p.workspace_path, &p.path, p.staged, p.source_id.as_deref())
                    .await?;
                Ok(Reply::of(&diff))
            }),
        ),
        (
            "getBranchComparison",
            call(&repo, |repo, p: RepoRequest| async move {
                Ok(Reply::Value(branch_comparison_value(
                    &repo.branch_comparison(&p.workspace_path).await?,
                )))
            }),
        ),
        (
            "stagePaths",
            call(&repo, |repo, p: PathsRequest| async move {
                repo.mutate_paths(
                    &p.workspace_path,
                    &p.paths,
                    "stage paths",
                    &["add"],
                    "git add",
                )
                .await?;
                Ok(Reply::Undefined)
            }),
        ),
        (
            "unstagePaths",
            call(&repo, |repo, p: PathsRequest| async move {
                repo.mutate_paths(
                    &p.workspace_path,
                    &p.paths,
                    "unstage paths",
                    &["restore", "--staged"],
                    "git restore --staged",
                )
                .await?;
                Ok(Reply::Undefined)
            }),
        ),
        (
            "discardPaths",
            call(&repo, |repo, p: PathsRequest| async move {
                // 只还原已跟踪文件；未跟踪文件不会被删除。
                let command: &[&str] = if p.staged.unwrap_or(false) {
                    &["restore", "--source=HEAD", "--staged", "--worktree"]
                } else {
                    &["restore", "--worktree"]
                };
                repo.mutate_paths(
                    &p.workspace_path,
                    &p.paths,
                    "discard paths",
                    command,
                    "git restore",
                )
                .await?;
                Ok(Reply::Undefined)
            }),
        ),
        (
            "commit",
            call(&repo, |repo, p: CommitRequest| async move {
                let hash = repo
                    .commit(
                        &p.workspace_path,
                        &p.message,
                        p.paths.as_deref(),
                        p.staged_only.unwrap_or(false),
                    )
                    .await?;
                let status = repo.status(&p.workspace_path).await?;
                Ok(Reply::Value(
                    json!({"commitHash": hash, "summary": status.summary}),
                ))
            }),
        ),
        (
            "push",
            call(&repo, |repo, p: RepoRequest| async move {
                Ok(Reply::Value(repo.push(&p.workspace_path).await?))
            }),
        ),
        (
            "getIdentity",
            call(&repo, |repo, p: RepoRequest| async move {
                Ok(Reply::Value(repo.identity(&p.workspace_path).await?))
            }),
        ),
        (
            "refresh",
            call(&repo, |repo, p: RefreshRequest| async move {
                let workspace = p.workspace_path.as_str();
                // 一次状态快照里切出 summary 与两份变更，避免 UI 并发三个 RPC 各跑一遍 git status。
                let (status, identity, comparison) = tokio::join!(
                    repo.status(workspace),
                    async {
                        match p.include_identity {
                            Some(true) => repo.identity(workspace).await.map(Some),
                            _ => Ok(None),
                        }
                    },
                    async {
                        match p.include_branch_comparison {
                            Some(true) => repo.branch_comparison(workspace).await.map(Some),
                            _ => Ok(None),
                        }
                    },
                );
                let (status, identity, comparison) = (status?, identity?, comparison?);
                Ok(Reply::Value(json!({
                    "summary": status.summary,
                    "identity": identity,
                    "unstagedChanges": changes_for_source(&status, false),
                    "stagedChanges": changes_for_source(&status, true),
                    "branchComparison": comparison.as_ref().map(branch_comparison_value),
                })))
            }),
        ),
    ];
    methods
        .into_iter()
        .fold(ServiceBuilder::new("git"), |builder, (name, handler)| {
            register(builder, name, handler)
        })
}
