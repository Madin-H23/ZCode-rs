//! git 输出解析（packages/services/src/git/repo/gitCliHelpers.ts、gitCliRepo.ts 中的纯函数）。
use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use zcode_host_kit::RpcError;

use super::command::{GitOutput, git_error};
use crate::support::collate::locale_compare;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HeadRefType {
    Branch,
    Detached,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusEntry {
    pub path: String,
    pub original_path: Option<String>,
    pub kind: ChangeKind,
    pub x: Option<String>,
    pub y: Option<String>,
    pub is_untracked: bool,
    pub is_conflicted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Porcelain {
    pub branch_name: Option<String>,
    pub tracking_branch_name: Option<String>,
    pub head_ref_type: HeadRefType,
    pub ahead: u64,
    pub behind: u64,
    pub entries: Vec<StatusEntry>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LineStat {
    pub added: u64,
    pub removed: u64,
    pub kind: Option<ChangeKind>,
    pub original_path: Option<String>,
}

/// numstat 结果；保留首次出现的顺序（JS `Map` 语义）。
#[derive(Debug, Clone, Default)]
pub struct NumStats {
    order: Vec<String>,
    stats: HashMap<String, LineStat>,
}

impl NumStats {
    fn set(&mut self, path: String, stat: LineStat) {
        if !self.stats.contains_key(&path) {
            self.order.push(path.clone());
        }
        self.stats.insert(path, stat);
    }

    pub fn get(&self, path: &str) -> Option<&LineStat> {
        self.stats.get(path)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &LineStat)> {
        self.order
            .iter()
            .filter_map(|path| self.stats.get(path).map(|stat| (path, stat)))
    }
}

pub fn normalize_git_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn strip_dot_slash(path: &str) -> &str {
    path.strip_prefix("./")
        .or_else(|| path.strip_prefix('/'))
        .unwrap_or(path)
}

/// `normalizeWorkspaceInRepoPath`：去掉开头的 `./` 或 `/` 与结尾的 `/`，空串为 `.`。
pub fn normalize_workspace_in_repo_path(path: &str) -> String {
    let normalized = normalize_git_path(path);
    let trimmed = strip_dot_slash(&normalized).trim_end_matches('/');
    if trimmed.is_empty() {
        ".".to_owned()
    } else {
        trimmed.to_owned()
    }
}

pub fn is_in_workspace_scope(repo_relative_path: &str, workspace_in_repo_path: &str) -> bool {
    let normalized = normalize_git_path(repo_relative_path);
    let path = strip_dot_slash(&normalized);
    let workspace = normalize_workspace_in_repo_path(workspace_in_repo_path);
    workspace == "."
        || path == workspace
        || path
            .strip_prefix(&workspace)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub fn to_workspace_relative(repo_relative_path: &str, workspace_in_repo_path: &str) -> String {
    let normalized = normalize_git_path(repo_relative_path);
    let path = strip_dot_slash(&normalized);
    let workspace = normalize_workspace_in_repo_path(workspace_in_repo_path);
    if workspace == "." {
        return path.to_owned();
    }
    if path == workspace {
        return ".".to_owned();
    }
    path.strip_prefix(&workspace)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
        .to_owned()
}

/// `Number.parseInt` 的前缀语义：跳过前导空白，取可选符号加连续数字。
pub fn parse_int(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().ok().map(|value| sign * value)
}

fn kind_from_status(code: char) -> ChangeKind {
    match code {
        'A' | '?' => ChangeKind::Added,
        'D' => ChangeKind::Deleted,
        'R' | 'C' => ChangeKind::Renamed,
        _ => ChangeKind::Modified,
    }
}

static AHEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\+(\d+)").expect("valid regex"));
static BEHIND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-(\d+)").expect("valid regex"));

// 路径部分用 JS `.` 的字符集（不含任何换行符），与 TS 正则的匹配范围一致。
static ORDINARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^1 ([^ ]{2})(?: [^ ]+){6} ([^\n\r\x{2028}\x{2029}]+)$").expect("valid regex")
});
static RENAMED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^2 ([^ ]{2})(?: [^ ]+){7} ([^\n\r\x{2028}\x{2029}]+)$").expect("valid regex")
});
static UNMERGED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^u ([^ ]{2})(?: [^ ]+){8} ([^\n\r\x{2028}\x{2029}]+)$").expect("valid regex")
});

fn xy(code: &str) -> (String, String) {
    let mut chars = code.chars();
    let x = chars.next().map(String::from).unwrap_or_default();
    let y = chars.next().map(String::from).unwrap_or_default();
    (x, y)
}

/// `git status --porcelain=v2 --branch -z`。
pub fn parse_status_porcelain(stdout: &str) -> Porcelain {
    let records: Vec<&str> = stdout
        .split('\0')
        .filter(|record| !record.is_empty())
        .collect();
    let mut porcelain = Porcelain {
        branch_name: None,
        tracking_branch_name: None,
        head_ref_type: HeadRefType::Branch,
        ahead: 0,
        behind: 0,
        entries: Vec::new(),
    };
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.starts_with("# ") {
            if let Some(head) = record.strip_prefix("# branch.head ") {
                if head == "(detached)" {
                    porcelain.branch_name = None;
                    porcelain.head_ref_type = HeadRefType::Detached;
                } else {
                    porcelain.branch_name = Some(head.to_owned());
                    porcelain.head_ref_type = HeadRefType::Branch;
                }
            } else if let Some(upstream) = record.strip_prefix("# branch.upstream ") {
                porcelain.tracking_branch_name = Some(upstream.to_owned());
            } else if let Some(counts) = record.strip_prefix("# branch.ab ") {
                let count = |pattern: &Regex| {
                    pattern
                        .captures(counts)
                        .and_then(|captures| captures[1].parse::<u64>().ok())
                        .unwrap_or(0)
                };
                porcelain.ahead = count(&AHEAD);
                porcelain.behind = count(&BEHIND);
            }
            continue;
        }
        if let Some(path) = record.strip_prefix("? ") {
            porcelain.entries.push(StatusEntry {
                path: normalize_git_path(path),
                original_path: None,
                kind: ChangeKind::Added,
                x: None,
                y: Some("?".to_owned()),
                is_untracked: true,
                is_conflicted: false,
            });
            continue;
        }
        let (pattern, renamed, conflicted) = match record.as_bytes().first() {
            Some(b'1') => (&*ORDINARY, false, false),
            Some(b'2') => (&*RENAMED, true, false),
            Some(b'u') => (&*UNMERGED, false, true),
            _ => continue,
        };
        let Some(captures) = pattern.captures(record) else {
            continue;
        };
        let (x, y) = xy(&captures[1]);
        let original_path = if renamed {
            // 重命名记录的下一条 NUL 记录是原路径。
            let original = records.get(index).map(|path| normalize_git_path(path));
            index += 1;
            original
        } else {
            None
        };
        let kind = if renamed {
            ChangeKind::Renamed
        } else if conflicted {
            ChangeKind::Modified
        } else {
            let code = if x != "." { &x } else { &y };
            kind_from_status(code.chars().next().unwrap_or(' '))
        };
        porcelain.entries.push(StatusEntry {
            path: normalize_git_path(&captures[2]),
            original_path,
            kind,
            x: Some(x),
            y: Some(y),
            is_untracked: false,
            is_conflicted: conflicted,
        });
    }
    porcelain
}

fn numstat_value(value: &str) -> u64 {
    if value == "-" {
        return 0;
    }
    parse_int(value)
        .map(|value| value.max(0) as u64)
        .unwrap_or(0)
}

/// `git diff --numstat -z`；路径为空的记录是重命名，后两条记录依次是原路径与新路径。
pub fn parse_numstat(stdout: &str) -> NumStats {
    let records: Vec<&str> = stdout.split('\0').collect();
    let mut stats = NumStats::default();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\t').collect();
        if fields.len() < 3 {
            continue;
        }
        let (added, removed) = (numstat_value(fields[0]), numstat_value(fields[1]));
        let path = fields[2..].join("\t");
        if !path.is_empty() {
            stats.set(
                normalize_git_path(&path),
                LineStat {
                    added,
                    removed,
                    ..LineStat::default()
                },
            );
            continue;
        }
        let original = records.get(index).copied().unwrap_or_default();
        let renamed = records.get(index + 1).copied().unwrap_or_default();
        index += 2;
        if renamed.is_empty() {
            continue;
        }
        stats.set(
            normalize_git_path(renamed),
            LineStat {
                added,
                removed,
                kind: Some(ChangeKind::Renamed),
                original_path: Some(normalize_git_path(original)),
            },
        );
    }
    stats
}

pub fn kind_from_numstat(stat: &LineStat) -> ChangeKind {
    if let Some(kind) = stat.kind {
        return kind;
    }
    match (stat.added > 0, stat.removed > 0) {
        (true, false) => ChangeKind::Added,
        (false, true) => ChangeKind::Deleted,
        _ => ChangeKind::Modified,
    }
}

fn normalized_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(|line| line.trim_end().to_owned())
        .collect()
}

fn indented_paths(lines: &[String], header: &Regex) -> Vec<String> {
    let Some(start) = lines
        .iter()
        .position(|line| header.is_match(&line.to_lowercase()))
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for line in &lines[start + 1..] {
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            break;
        }
        let value = line.trim();
        if !value.is_empty() {
            paths.push(normalize_git_path(value));
        }
    }
    paths
}

static TRACKED_OVERWRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"your local changes to the following files would be overwritten by (checkout|switch)",
    )
    .expect("valid regex")
});
static UNTRACKED_OVERWRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"the following untracked working tree files would be overwritten by (checkout|switch)",
    )
    .expect("valid regex")
});
static OPERATION_BLOCKED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"cannot switch branch while (merging|rebasing|cherry-picking|reverting|bisecting)")
        .expect("valid regex")
});

pub fn invalid_branch_name_issue(detail: Option<&str>) -> Value {
    let detail = detail.map(str::trim).filter(|detail| !detail.is_empty());
    json!({"code": "invalid-branch-name", "message": "Branch name is invalid.", "detail": detail})
}

/// `parseGitBranchMutationIssues`：把 switch 的英文 stderr 归一成稳定的 issue code（首个命中）。
pub fn parse_branch_mutation_issues(output: &GitOutput) -> Vec<Value> {
    let stderr = output.stderr.trim();
    let stdout = output.stdout.trim();
    let detail = if !stderr.is_empty() {
        Some(stderr)
    } else if !stdout.is_empty() {
        Some(stdout)
    } else {
        None
    };
    let lines = normalized_lines(detail.unwrap_or_default());
    let lowered = detail.unwrap_or_default().to_lowercase();
    let issue =
        |code: &str, message: &str| json!([{"code": code, "message": message, "detail": detail}]);

    let tracked = indented_paths(&lines, &TRACKED_OVERWRITE);
    if !tracked.is_empty() {
        return vec![json!({
            "code": "tracked-changes-would-be-overwritten",
            "message": "Tracked changes would be overwritten by switching branches.",
            "paths": tracked,
            "detail": detail,
        })];
    }
    let untracked = indented_paths(&lines, &UNTRACKED_OVERWRITE);
    if !untracked.is_empty() {
        return vec![json!({
            "code": "untracked-changes-would-be-overwritten",
            "message": "Untracked files would be overwritten by switching branches.",
            "paths": untracked,
            "detail": detail,
        })];
    }
    let found = if lowered.contains("already exists") {
        issue("branch-already-exists", "Branch already exists.")
    } else if lowered.contains("invalid reference:") {
        issue("target-branch-not-found", "Target branch was not found.")
    } else if lowered.contains("is already used by worktree at") {
        issue(
            "branch-in-other-worktree",
            "Branch is already checked out in another worktree.",
        )
    } else if lowered.contains("resolve your current index first") {
        issue(
            "conflicts-present",
            "Repository still has unresolved conflicts.",
        )
    } else if OPERATION_BLOCKED.is_match(&lowered)
        || lowered.contains("you have not concluded your merge")
        || lowered.contains("rebase in progress")
    {
        issue(
            "operation-in-progress",
            "Another Git operation is still in progress.",
        )
    } else {
        issue("unknown", "Git could not complete the branch operation.")
    };
    match found {
        Value::Array(issues) => issues,
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    pub path: String,
    pub availability: &'static str,
    pub patch: Option<String>,
    pub before_content: Option<String>,
    pub after_content: Option<String>,
    pub summary: Option<String>,
}

pub const BINARY_SUMMARY: &str = "Binary diff is not previewable.";
pub const TRUNCATED_SUMMARY: &str = "Git diff output exceeded the preview limit.";

impl DiffResult {
    pub fn unavailable(path: &str, summary: impl Into<String>) -> Self {
        Self::without_patch(path, "unavailable", summary)
    }

    pub fn without_patch(
        path: &str,
        availability: &'static str,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            path: path.to_owned(),
            availability,
            patch: None,
            before_content: None,
            after_content: None,
            summary: Some(summary.into()),
        }
    }

    /// 完整内容必须两侧都读到才附带，否则 UI 会把“读不到”当成空文件。
    pub fn with_contents(mut self, contents: Option<(String, String)>) -> Self {
        if self.availability == "patch"
            && let Some((before, after)) = contents
        {
            self.before_content = Some(before);
            self.after_content = Some(after);
        }
        self
    }
}

/// `toDiffResult`。
pub fn to_diff_result(
    path: &str,
    output: &GitOutput,
    allowed: &[i64],
    empty_summary: &str,
) -> DiffResult {
    if output.timed_out {
        return DiffResult::unavailable(path, "Git diff command timed out.");
    }
    if output.output_truncated {
        return DiffResult::without_patch(path, "truncated", TRUNCATED_SUMMARY);
    }
    if !output.exit_code.is_some_and(|code| allowed.contains(&code)) {
        return DiffResult::unavailable(path, output.message());
    }
    if output.stdout.trim().is_empty() {
        return DiffResult::unavailable(path, empty_summary);
    }
    if output.stdout.contains("GIT binary patch") || output.stdout.contains("Binary files ") {
        return DiffResult::without_patch(path, "binary", BINARY_SUMMARY);
    }
    DiffResult {
        path: path.to_owned(),
        availability: "patch",
        patch: Some(output.stdout.clone()),
        before_content: None,
        after_content: None,
        summary: None,
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigValue {
    pub scope: Option<String>,
    pub source: Option<String>,
    pub value: Option<String>,
}

/// `git config --show-scope --show-origin --get`：`scope\torigin\tvalue`，退出码 1 表示未设置。
pub fn parse_config_value(output: &GitOutput) -> Result<ConfigValue, RpcError> {
    if output.exit_code == Some(1) {
        return Ok(ConfigValue::default());
    }
    output.ensure("git config")?;
    let line = output
        .stdout
        .strip_suffix('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .unwrap_or(&output.stdout);
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 3 {
        return Ok(ConfigValue {
            value: (!line.is_empty()).then(|| line.to_owned()),
            ..ConfigValue::default()
        });
    }
    let value = parts[2..].join("\t");
    Ok(ConfigValue {
        scope: Some(parts[0].to_owned()),
        source: Some(parts[1].to_owned()),
        value: (!value.is_empty()).then_some(value),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalBranch {
    pub name: String,
    pub is_current: bool,
    pub upstream_name: Option<String>,
    pub commit_hash: Option<String>,
    pub commit_timestamp_ms: Option<i64>,
}

/// `for-each-ref refs/heads`：当前分支在前，其后按提交时间倒序（无时间的最后），再按名称。
pub fn parse_branch_refs(stdout: &str, current: Option<&str>) -> Vec<LocalBranch> {
    let mut branches: Vec<LocalBranch> = stdout
        .replace("\r\n", "\n")
        .split('\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let name = fields.next().filter(|name| !name.is_empty())?.to_owned();
            let non_empty =
                |value: Option<&str>| value.filter(|v| !v.is_empty()).map(str::to_owned);
            let upstream_name = non_empty(fields.next());
            let commit_hash = non_empty(fields.next());
            let commit_timestamp_ms = fields
                .next()
                .filter(|value| !value.is_empty())
                .and_then(parse_int)
                .map(|seconds| seconds * 1000);
            Some(LocalBranch {
                is_current: Some(name.as_str()) == current,
                name,
                upstream_name,
                commit_hash,
                commit_timestamp_ms,
            })
        })
        .collect();
    branches.sort_by(|left, right| {
        right
            .is_current
            .cmp(&left.is_current)
            .then_with(
                || match (left.commit_timestamp_ms, right.commit_timestamp_ms) {
                    (Some(left), Some(right)) => right.cmp(&left),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                },
            )
            .then_with(|| locale_compare(&left.name, &right.name))
    });
    branches
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GraphRef {
    pub name: String,
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphCommit {
    pub hash: String,
    pub parents: Vec<String>,
    pub refs: Vec<GraphRef>,
    pub subject: String,
    pub author_name: Option<String>,
    pub authored_at_ms: Option<i64>,
}

fn decoration_ref(raw: &str) -> Option<GraphRef> {
    let reference = raw.trim();
    let named = |name: &str, kind| {
        (!name.is_empty()).then(|| GraphRef {
            name: name.to_owned(),
            kind,
        })
    };
    if reference.is_empty() {
        return None;
    }
    if reference == "HEAD" {
        return named("HEAD", "head");
    }
    if let Some(tag) = reference.strip_prefix("tag: ") {
        let tag = tag.trim();
        return named(tag.strip_prefix("refs/tags/").unwrap_or(tag), "tag");
    }
    for (prefix, kind) in [
        ("refs/heads/", "branch"),
        ("refs/remotes/", "remote"),
        ("refs/tags/", "tag"),
    ] {
        if let Some(name) = reference.strip_prefix(prefix) {
            return named(name, kind);
        }
    }
    named(
        reference,
        if reference.contains('/') {
            "remote"
        } else {
            "branch"
        },
    )
}

fn graph_refs(decorations: &str) -> Vec<GraphRef> {
    let mut refs: Vec<GraphRef> = Vec::new();
    let mut add = |reference: GraphRef| {
        if !refs.contains(&reference) {
            refs.push(reference);
        }
    };
    for decoration in decorations
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        if let Some(pointed) = decoration.strip_prefix("HEAD -> ") {
            add(GraphRef {
                name: "HEAD".to_owned(),
                kind: "head",
            });
            if let Some(reference) = decoration_ref(pointed) {
                add(reference);
            }
            continue;
        }
        if let Some(reference) = decoration_ref(decoration) {
            add(reference);
        }
    }
    refs
}

/// `git log --format=%H%x00%P%x00%an%x00%at%x00%s%x00%D%x1e`。
pub fn parse_graph_records(stdout: &str) -> Vec<GraphCommit> {
    stdout
        .split('\x1e')
        .map(str::trim)
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let fields: Vec<&str> = record.split('\0').collect();
            let field = |index: usize| fields.get(index).copied();
            let hash = field(0).filter(|hash| !hash.is_empty())?.to_owned();
            Some(GraphCommit {
                hash,
                parents: field(1)
                    .map(|parents| {
                        parents
                            .split(' ')
                            .filter(|p| !p.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                refs: graph_refs(field(5).unwrap_or_default()),
                subject: field(4).unwrap_or_default().to_owned(),
                author_name: field(2).filter(|name| !name.is_empty()).map(str::to_owned),
                authored_at_ms: field(3)
                    .filter(|value| !value.is_empty())
                    .and_then(parse_int)
                    .map(|seconds| seconds * 1000),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    pub mode: String,
    pub object_hash: String,
    pub stage: String,
    pub path: String,
}

/// `git ls-files --stage -z`：`<mode> <hash> <stage>\t<path>`。
pub fn parse_index_entries(stdout: &str) -> Result<Vec<IndexEntry>, RpcError> {
    let invalid = || git_error("Failed to parse staged Git index entry.");
    stdout
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let (meta, path) = record.split_once('\t').ok_or_else(invalid)?;
            let mut fields = meta.split_whitespace();
            let (Some(mode), Some(object_hash), Some(stage)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(invalid());
            };
            let path = normalize_git_path(path);
            if path.is_empty() {
                return Err(invalid());
            }
            Ok(IndexEntry {
                mode: mode.to_owned(),
                object_hash: object_hash.to_owned(),
                stage: stage.to_owned(),
                path,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let stdout = [
            "# branch.oid abc",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 .M N... 100644 100644 100644 a b src/a.ts",
            "1 A. N... 000000 100644 100644 0 b new file.txt",
            "2 R. N... 100644 100644 100644 a b R100 new.ts",
            "old.ts",
            "u UU N... 100644 100644 100644 100644 a b c conflict.ts",
            "? untracked/",
            "! ignored.log",
            "",
        ]
        .join("\0");
        let parsed = parse_status_porcelain(&stdout);
        assert_eq!(parsed.branch_name.as_deref(), Some("main"));
        assert_eq!(parsed.tracking_branch_name.as_deref(), Some("origin/main"));
        assert_eq!((parsed.ahead, parsed.behind), (2, 1));
        let summary: Vec<_> = parsed
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.path.as_str(),
                    entry.kind,
                    entry.original_path.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("src/a.ts", ChangeKind::Modified, None),
                ("new file.txt", ChangeKind::Added, None),
                ("new.ts", ChangeKind::Renamed, Some("old.ts")),
                ("conflict.ts", ChangeKind::Modified, None),
                ("untracked/", ChangeKind::Added, None),
            ]
        );
        assert!(parsed.entries[3].is_conflicted);
        assert!(parsed.entries[4].is_untracked);
        assert_eq!(parsed.entries[4].y.as_deref(), Some("?"));
        let detached = parse_status_porcelain("# branch.head (detached)\0");
        assert_eq!(detached.head_ref_type, HeadRefType::Detached);
        assert_eq!(detached.branch_name, None);
    }

    #[test]
    fn parses_numstat_with_renames() {
        let stats = parse_numstat("3\t1\tsrc/a.ts\0-\t-\tbin.png\0");
        assert_eq!(
            stats.get("src/a.ts").map(|s| (s.added, s.removed)),
            Some((3, 1))
        );
        assert_eq!(
            stats.get("bin.png").map(|s| (s.added, s.removed)),
            Some((0, 0))
        );
        let renamed = parse_numstat("1\t0\t\0old.ts\0new.ts\0");
        let stat = renamed.get("new.ts").unwrap();
        assert_eq!(stat.kind, Some(ChangeKind::Renamed));
        assert_eq!(stat.original_path.as_deref(), Some("old.ts"));
        assert_eq!(
            kind_from_numstat(&LineStat {
                added: 2,
                ..LineStat::default()
            }),
            ChangeKind::Added
        );
    }

    #[test]
    fn workspace_scope_helpers() {
        assert_eq!(normalize_workspace_in_repo_path("./pkg/app/"), "pkg/app");
        assert_eq!(normalize_workspace_in_repo_path(""), ".");
        assert!(is_in_workspace_scope("pkg/app/x.ts", "pkg/app"));
        assert!(!is_in_workspace_scope("pkg/apple.ts", "pkg/app"));
        assert_eq!(to_workspace_relative("pkg/app/x.ts", "pkg/app"), "x.ts");
        assert_eq!(to_workspace_relative("pkg/app", "pkg/app"), ".");
    }

    #[test]
    fn classifies_branch_switch_errors() {
        let stderr = "error: Your local changes to the following files would be overwritten by checkout:\n\tsrc/a.ts\n\tsrc\\b.ts\nPlease commit your changes or stash them before you switch branches.\nAborting\n";
        let issues = parse_branch_mutation_issues(&GitOutput::exited(1, "", stderr));
        assert_eq!(issues[0]["code"], "tracked-changes-would-be-overwritten");
        assert_eq!(issues[0]["paths"], json!(["src/a.ts", "src/b.ts"]));
        let missing = parse_branch_mutation_issues(&GitOutput::exited(
            128,
            "",
            "fatal: invalid reference: nope\n",
        ));
        assert_eq!(missing[0]["code"], "target-branch-not-found");
        assert_eq!(missing[0]["detail"], "fatal: invalid reference: nope");
        let unknown = parse_branch_mutation_issues(&GitOutput::exited(1, "", ""));
        assert_eq!(
            unknown[0],
            json!({"code": "unknown", "message": "Git could not complete the branch operation.", "detail": null})
        );
    }

    #[test]
    fn parses_config_values_and_graph() {
        let value = parse_config_value(&GitOutput::exited(
            0,
            "global\tfile:/home/u/.gitconfig\tJane\tDoe\n",
            "",
        ))
        .unwrap();
        assert_eq!(value.scope.as_deref(), Some("global"));
        assert_eq!(value.value.as_deref(), Some("Jane\tDoe"));
        assert_eq!(
            parse_config_value(&GitOutput::exited(1, "", "")).unwrap(),
            ConfigValue::default()
        );

        let commits = parse_graph_records(
            "abc\0p1 p2\0Jane\x001700000000\0subject\0HEAD -> main, origin/main, tag: v1\x1e\ndef\0\0\0\0root\0\x1e\n",
        );
        assert_eq!(commits.len(), 2);
        let names: Vec<_> = commits[0]
            .refs
            .iter()
            .map(|r| (r.name.as_str(), r.kind))
            .collect();
        assert_eq!(
            names,
            vec![
                ("HEAD", "head"),
                ("main", "branch"),
                ("origin/main", "remote"),
                ("v1", "tag")
            ]
        );
        assert_eq!(commits[0].authored_at_ms, Some(1_700_000_000_000));
        assert_eq!(commits[1].author_name, None);
        assert!(commits[1].parents.is_empty());
    }

    #[test]
    fn sorts_branches_current_first_then_recent() {
        let stdout =
            "old\0\0h1\x00100\nmain\0origin/main\0h2\x00200\nnew\0\0h3\x00300\nzeta\0\0h4\0\n";
        let names: Vec<_> = parse_branch_refs(stdout, Some("main"))
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert_eq!(names, vec!["main", "new", "old", "zeta"]);
    }

    #[test]
    fn parses_index_entries() {
        let entries = parse_index_entries("100644 abc 0\tsrc/a.ts\0").unwrap();
        assert_eq!(entries[0].path, "src/a.ts");
        assert!(parse_index_entries("garbage\0").is_err());
    }
}
