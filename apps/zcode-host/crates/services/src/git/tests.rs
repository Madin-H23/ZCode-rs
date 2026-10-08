//! 用真实 git 仓库（临时目录）验证 `git` channel 的返回形态与边界。
//! git 的全局配置指向临时文件，测试不读开发者的 ~/.gitconfig，也不写任何真实目录。
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::{Value, json};
use zcode_host_kit::ServiceChannel;

use super::repo::GitRepo;
use super::service;
use crate::support::env::ProcessEnv;
use crate::testing::{call, channel};

const TEST_PATH: &str = "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin";

struct Fixture {
    _home: tempfile::TempDir,
    env: ProcessEnv,
    git: Arc<dyn ServiceChannel>,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("gitconfig");
        std::fs::write(
            &config,
            "[user]\n\tname = Test User\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n[commit]\n\tgpgsign = false\n",
        )
        .unwrap();
        let env = ProcessEnv::new(
            [
                ("PATH", TEST_PATH),
                ("HOME", home.path().to_str().unwrap()),
                ("GIT_CONFIG_GLOBAL", config.to_str().unwrap()),
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("TMPDIR", home.path().to_str().unwrap()),
            ]
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        );
        let git = channel(service(GitRepo::new(&env)));
        Self {
            _home: home,
            env,
            git,
        }
    }

    fn sh(&self, cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(self.env.vars())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repo(&self) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        // macOS 的临时目录经过 /var → /private/var 符号链接；用规范路径，git 也这样报告仓库根。
        let root = dir.path().canonicalize().unwrap();
        self.sh(&root, &["init", "-q", "-b", "main"]);
        (dir, root)
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        call(&self.git, method, params)
            .await
            .unwrap_or_else(|error| panic!("{method} failed: {}", error.message))
    }

    async fn fail(&self, method: &str, params: Value) -> String {
        call(&self.git, method, params).await.unwrap_err().message
    }
}

fn ws(root: &Path) -> String {
    root.to_str().unwrap().to_owned()
}

#[tokio::test]
async fn not_a_repository_is_data_for_reads_and_an_error_for_mutations() {
    let fixture = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let workspace = ws(dir.path());
    let summary = fixture
        .call("getRepositorySummary", json!({"workspacePath": workspace}))
        .await;
    assert_eq!(summary["isRepository"], false);
    assert_eq!(summary["isGitAvailable"], true);
    assert_eq!(summary["repoRoot"], workspace.as_str());
    assert_eq!(summary["workspaceInRepoPath"], ".");
    assert_eq!(summary["headRefType"], "branch");
    assert_eq!(summary["autoRefreshWatchPaths"], json!([]));

    let refreshed = fixture
        .call(
            "refresh",
            json!({"workspacePath": workspace, "includeIdentity": true}),
        )
        .await;
    assert_eq!(refreshed["unstagedChanges"], json!([]));
    assert_eq!(refreshed["identity"]["userName"], Value::Null);
    assert_eq!(refreshed["branchComparison"], Value::Null);
    let diff = fixture
        .call(
            "getDiff",
            json!({"workspacePath": workspace, "path": "a.txt"}),
        )
        .await;
    assert_eq!(diff["availability"], "unavailable");
    assert_eq!(diff["summary"], "Workspace is not inside a Git repository.");
    assert_eq!(
        fixture
            .call(
                "getIgnoredPaths",
                json!({"workspacePath": workspace, "paths": ["a"]})
            )
            .await,
        json!([])
    );
    assert_eq!(
        fixture
            .fail(
                "stagePaths",
                json!({"workspacePath": workspace, "paths": ["a"]})
            )
            .await,
        "Cannot stage paths: workspace is not inside a Git repository"
    );

    let missing = fixture
        .call(
            "getRepositorySummary",
            json!({"workspacePath": format!("{workspace}/gone")}),
        )
        .await;
    assert_eq!(missing["isRepository"], false);
    assert_eq!(missing["isGitAvailable"], true);
}

#[tokio::test]
async fn missing_git_binary_reports_unavailable() {
    let env = ProcessEnv::new([("PATH".to_owned(), "/nonexistent".to_owned())].into());
    let git = channel(service(GitRepo::new(&env)));
    let summary = call(
        &git,
        "getRepositorySummary",
        json!({"workspacePath": "/tmp"}),
    )
    .await
    .unwrap();
    assert_eq!(summary["isGitAvailable"], false);
    assert_eq!(summary["isRepository"], false);
    let diff = call(
        &git,
        "getDiff",
        json!({"workspacePath": "/tmp", "path": "x"}),
    )
    .await
    .unwrap();
    assert_eq!(
        diff["summary"],
        "Git binary is not available in the current environment."
    );
    let error = call(
        &git,
        "commit",
        json!({"workspacePath": "/tmp", "message": "x"}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.message,
        "Cannot commit changes: Git binary is not available"
    );
}

#[tokio::test]
async fn status_stage_diff_and_commit_flow() {
    let fixture = Fixture::new();
    let (_dir, root) = fixture.repo();
    let workspace = ws(&root);
    std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(root.join("empty.txt"), "").unwrap();
    std::fs::write(root.join("bin.dat"), [0u8, 1, 2]).unwrap();

    let refreshed = fixture
        .call("refresh", json!({"workspacePath": workspace}))
        .await;
    let summary = &refreshed["summary"];
    assert_eq!(summary["isRepository"], true);
    assert_eq!(summary["branchName"], "main");
    assert_eq!(summary["isDirty"], true);
    assert_eq!(summary["repoRoot"], workspace.as_str());
    assert_eq!(
        summary["autoRefreshWatchPaths"],
        json!([{"path": format!("{workspace}/.git"), "recursive": true}])
    );
    // 空文件与二进制未跟踪文件没有可审查的行，不出现在列表里。
    assert_eq!(
        refreshed["unstagedChanges"],
        json!([{
            "path": root.join("a.txt").to_str().unwrap(),
            "repoRelativePath": "a.txt",
            "workspaceRelativePath": "a.txt",
            "y": "?",
            "kind": "added",
            "section": "untracked",
            "added": 2,
            "removed": 0,
            "isStaged": false,
            "isUntracked": true,
            "isConflicted": false,
        }])
    );
    assert_eq!(refreshed["identity"], Value::Null);

    let untracked = fixture
        .call(
            "getDiff",
            json!({"workspacePath": workspace, "path": "a.txt"}),
        )
        .await;
    assert_eq!(untracked["availability"], "patch");
    assert_eq!(
        untracked["patch"],
        "--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n"
    );
    assert_eq!(untracked["beforeContent"], "");
    let binary = fixture
        .call(
            "getDiff",
            json!({"workspacePath": workspace, "path": "bin.dat"}),
        )
        .await;
    assert_eq!(binary["availability"], "binary");

    fixture
        .call(
            "stagePaths",
            json!({"workspacePath": workspace, "paths": ["a.txt", "a.txt"]}),
        )
        .await;
    let staged = fixture
        .call(
            "getChanges",
            json!({"workspacePath": workspace, "sourceId": "staged"}),
        )
        .await;
    assert_eq!(staged[0]["section"], "staged");
    assert_eq!(staged[0]["x"], "A");
    assert_eq!(staged[0]["y"], ".");
    assert_eq!(staged[0]["added"], 2);
    let staged_diff = fixture
        .call(
            "getDiff",
            json!({"workspacePath": workspace, "path": "a.txt", "sourceId": "staged"}),
        )
        .await;
    assert_eq!(staged_diff["availability"], "patch");
    // HEAD 还不存在，两侧全文读不全时只给 patch。
    assert_eq!(staged_diff["beforeContent"], Value::Null);

    assert_eq!(
        fixture
            .fail(
                "commit",
                json!({"workspacePath": workspace, "message": "  "})
            )
            .await,
        "Commit message cannot be empty"
    );
    let committed = fixture
        .call(
            "commit",
            json!({"workspacePath": workspace, "message": "feat: first"}),
        )
        .await;
    let hash = committed["commitHash"].as_str().unwrap().to_owned();
    assert_eq!(hash, fixture.sh(&root, &["rev-parse", "HEAD"]));
    assert_eq!(committed["summary"]["branchName"], "main");

    std::fs::write(root.join("a.txt"), "one\nTWO\n").unwrap();
    let diff = fixture
        .call(
            "getDiff",
            json!({"workspacePath": workspace, "path": root.join("a.txt")}),
        )
        .await;
    assert_eq!(diff["availability"], "patch");
    assert_eq!(diff["beforeContent"], "one\ntwo\n");
    assert_eq!(diff["afterContent"], "one\nTWO\n");
    assert!(diff["patch"].as_str().unwrap().contains("+TWO"));
    let unstaged = fixture
        .call(
            "getChanges",
            json!({"workspacePath": workspace, "sourceId": "unstaged"}),
        )
        .await;
    assert_eq!(unstaged[0]["section"], "unstaged");
    assert_eq!(
        (
            unstaged[0]["added"].as_u64(),
            unstaged[0]["removed"].as_u64()
        ),
        (Some(1), Some(1))
    );

    fixture
        .call(
            "discardPaths",
            json!({"workspacePath": workspace, "paths": ["a.txt"]}),
        )
        .await;
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert_eq!(
        fixture
            .fail(
                "stagePaths",
                json!({"workspacePath": workspace, "paths": ["../outside"]})
            )
            .await,
        "Path is outside repository scope: ../outside"
    );

    let identity = fixture
        .call("getIdentity", json!({"workspacePath": workspace}))
        .await;
    assert_eq!(identity["userName"], "Test User");
    assert_eq!(identity["scopeLabel"], "global");
    assert!(
        identity["nameSource"]
            .as_str()
            .unwrap()
            .starts_with("file:")
    );

    let graph = fixture
        .call(
            "getCommitGraph",
            json!({"workspacePath": workspace, "maxCount": 0}),
        )
        .await;
    assert_eq!(graph["hasMore"], false);
    assert_eq!(graph["commits"][0]["hash"], hash.as_str());
    assert_eq!(graph["commits"][0]["subject"], "feat: first");
    assert_eq!(
        graph["commits"][0]["refs"],
        json!([{"name": "HEAD", "kind": "head"}, {"name": "main", "kind": "branch"}])
    );
}

#[tokio::test]
async fn staged_only_commit_keeps_other_staged_paths() {
    let fixture = Fixture::new();
    let (_dir, root) = fixture.repo();
    let workspace = ws(&root);
    std::fs::write(root.join("base.txt"), "base\n").unwrap();
    fixture.sh(&root, &["add", "."]);
    fixture.sh(&root, &["commit", "-qm", "base"]);
    std::fs::write(root.join("x.txt"), "x\n").unwrap();
    std::fs::write(root.join("y.txt"), "y\n").unwrap();
    fixture.sh(&root, &["add", "x.txt", "y.txt"]);
    // x 暂存后又改了工作区：只提交暂存版本。
    std::fs::write(root.join("x.txt"), "x changed\n").unwrap();

    fixture
        .call("commit", json!({"workspacePath": workspace, "message": "feat: x", "paths": ["x.txt"], "stagedOnly": true}))
        .await;
    assert_eq!(fixture.sh(&root, &["show", "HEAD:x.txt"]), "x");
    assert_eq!(
        fixture.sh(&root, &["diff", "--cached", "--name-only"]),
        "y.txt"
    );
    assert_eq!(fixture.sh(&root, &["diff", "--name-only"]), "x.txt");
    // 临时 index 目录已清理。
    let leftovers = std::fs::read_dir(fixture._home.path())
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("zcode-git-index-")
        })
        .count();
    assert_eq!(leftovers, 0);
}

#[tokio::test]
async fn branches_switching_and_errors() {
    let fixture = Fixture::new();
    let (_dir, root) = fixture.repo();
    let workspace = ws(&root);
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    fixture.sh(&root, &["add", "."]);
    fixture.sh(&root, &["commit", "-qm", "init"]);

    let created = fixture
        .call(
            "createBranchAndSwitch",
            json!({"workspacePath": workspace, "branchName": " feature ", "startPoint": "  "}),
        )
        .await;
    assert_eq!(created["ok"], true);
    assert_eq!(created["action"], "create-and-switch");
    assert_eq!(created["created"], true);
    assert_eq!(created["branchName"], "feature");
    assert_eq!(created["summary"]["branchName"], "feature");

    let same = fixture
        .call(
            "switchBranch",
            json!({"workspacePath": workspace, "targetBranchName": "feature"}),
        )
        .await;
    assert_eq!(
        (same["ok"].clone(), same["didChange"].clone()),
        (json!(true), json!(false))
    );

    let missing = fixture
        .call(
            "switchBranch",
            json!({"workspacePath": workspace, "targetBranchName": "nope"}),
        )
        .await;
    assert_eq!(missing["ok"], false);
    assert_eq!(missing["issues"][0]["code"], "target-branch-not-found");
    assert_eq!(missing["summary"]["branchName"], "feature");

    let invalid = fixture
        .call(
            "switchBranch",
            json!({"workspacePath": workspace, "targetBranchName": "a..b"}),
        )
        .await;
    assert_eq!(invalid["issues"][0]["code"], "invalid-branch-name");
    let empty = fixture
        .call(
            "switchBranch",
            json!({"workspacePath": workspace, "targetBranchName": " "}),
        )
        .await;
    assert_eq!(empty["branchName"], Value::Null);
    assert_eq!(empty["issues"][0]["detail"], Value::Null);

    let exists = fixture
        .call(
            "createBranchAndSwitch",
            json!({"workspacePath": workspace, "branchName": "main"}),
        )
        .await;
    assert_eq!(exists["issues"][0]["code"], "branch-already-exists");

    // 未提交的改动会被切换覆盖：git 拒绝并列出路径。
    fixture.sh(&root, &["switch", "-q", "main"]);
    std::fs::write(root.join("a.txt"), "main change\n").unwrap();
    fixture.sh(&root, &["commit", "-qam", "main change"]);
    std::fs::write(root.join("a.txt"), "dirty\n").unwrap();
    let blocked = fixture
        .call(
            "switchBranch",
            json!({"workspacePath": workspace, "targetBranchName": "feature"}),
        )
        .await;
    assert_eq!(
        blocked["issues"][0]["code"],
        "tracked-changes-would-be-overwritten"
    );
    assert_eq!(blocked["issues"][0]["paths"], json!(["a.txt"]));

    let branches = fixture
        .call("getLocalBranches", json!({"workspacePath": workspace}))
        .await;
    assert_eq!(branches["currentBranchName"], "main");
    assert_eq!(branches["headRefType"], "branch");
    assert_eq!(branches["branches"][0]["name"], "main");
    assert_eq!(branches["branches"][0]["isCurrent"], true);
    assert_eq!(branches["branches"][1]["name"], "feature");
    assert!(
        branches["branches"][1]["commitTimestampMs"]
            .as_i64()
            .unwrap()
            > 0
    );
}

#[tokio::test]
async fn ignored_paths_and_subdirectory_workspaces() {
    let fixture = Fixture::new();
    let (_dir, root) = fixture.repo();
    std::fs::create_dir_all(root.join("pkg/app")).unwrap();
    std::fs::write(root.join(".gitignore"), "*.log\nbuild/\n").unwrap();
    std::fs::write(root.join("pkg/app/x.ts"), "x\n").unwrap();
    std::fs::write(root.join("pkg/other.ts"), "o\n").unwrap();
    std::fs::write(root.join("pkg/app/debug.log"), "log\n").unwrap();

    let workspace = ws(&root.join("pkg/app"));
    let summary = fixture
        .call("getRepositorySummary", json!({"workspacePath": workspace}))
        .await;
    assert_eq!(summary["workspaceInRepoPath"], "pkg/app");
    assert_eq!(summary["repoRoot"], ws(&root).as_str());
    let changes = fixture
        .call(
            "getChanges",
            json!({"workspacePath": workspace, "sourceId": "unstaged"}),
        )
        .await;
    let names: Vec<_> = changes
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["workspaceRelativePath"].clone())
        .collect();
    assert_eq!(names, vec![json!("x.ts")]);

    let ignored = fixture
        .call(
            "getIgnoredPaths",
            json!({"workspacePath": workspace, "paths": ["debug.log", "x.ts", root.join("pkg/app/debug.log"), "../../../outside"]}),
        )
        .await;
    let debug_log = root.join("pkg/app/debug.log");
    assert_eq!(
        ignored,
        json!([debug_log.to_str().unwrap(), debug_log.to_str().unwrap()])
    );

    let info = fixture
        .call(
            "getWorkspaceRepositoryInfo",
            json!({"workspacePath": workspace}),
        )
        .await;
    assert_eq!(info["kind"], "main-tree");
}

#[tokio::test]
async fn push_sets_upstream_and_branch_comparison_follows_it() {
    let fixture = Fixture::new();
    let (_dir, root) = fixture.repo();
    let workspace = ws(&root);
    let remote = tempfile::tempdir().unwrap();
    fixture.sh(remote.path(), &["init", "-q", "--bare"]);
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    fixture.sh(&root, &["add", "."]);
    fixture.sh(&root, &["commit", "-qm", "init"]);

    assert_eq!(
        fixture
            .fail("push", json!({"workspacePath": workspace}))
            .await,
        "No Git remote is configured for the current repository."
    );
    fixture.sh(
        &root,
        &["remote", "add", "upstream", remote.path().to_str().unwrap()],
    );
    let pushed = fixture
        .call("push", json!({"workspacePath": workspace}))
        .await;
    assert_eq!(pushed["setUpstream"], true);
    assert_eq!(pushed["remoteName"], "upstream");
    assert_eq!(pushed["trackingBranchName"], "upstream/main");

    std::fs::write(root.join("b.txt"), "b\nb\n").unwrap();
    fixture.sh(&root, &["add", "."]);
    fixture.sh(&root, &["commit", "-qm", "second"]);
    let refreshed = fixture
        .call(
            "refresh",
            json!({"workspacePath": workspace, "includeBranchComparison": true}),
        )
        .await;
    assert_eq!(refreshed["summary"]["ahead"], 1);
    let comparison = &refreshed["branchComparison"];
    assert_eq!(comparison["comparisonLabel"], "main -> upstream/main");
    assert_eq!(comparison["changes"][0]["repoRelativePath"], "b.txt");
    assert_eq!(comparison["changes"][0]["section"], "branch");
    assert_eq!(comparison["changes"][0]["kind"], "added");
    let branch_diff = fixture
        .call(
            "getDiff",
            json!({"workspacePath": workspace, "path": "b.txt", "sourceId": "branch"}),
        )
        .await;
    assert_eq!(branch_diff["availability"], "patch");
    assert_eq!(branch_diff["beforeContent"], Value::Null);

    let again = fixture
        .call("push", json!({"workspacePath": workspace}))
        .await;
    assert_eq!(again["setUpstream"], false);
    assert_eq!(again["summary"]["ahead"], 0);
}
