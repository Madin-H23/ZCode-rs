//! 用真实 git 仓库（临时目录）验证 `git-checkpoint`：创建、diff、无冲突恢复、冲突拒绝、
//! force 覆盖、删除。git 全局配置与数据目录都在临时目录里，不读写开发者的真实目录。
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use zcode_host_kit::ServiceChannel;

use super::checkpoint::{Checkpoints, service};
use super::repo::GitRepo;
use crate::support::env::ProcessEnv;
use crate::testing::{call, channel};

const TEST_PATH: &str = "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin";

struct Fixture {
    home: tempfile::TempDir,
    env: ProcessEnv,
    channel: Arc<dyn ServiceChannel>,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("gitconfig");
        std::fs::write(
            &config,
            "[user]\n\tname = T\n\temail = t@example.com\n[init]\n\tdefaultBranch = main\n[commit]\n\tgpgsign = false\n",
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
        let data = home.path().join(".zcode");
        let checkpoints = Checkpoints::new(
            &zcode_host_base::testing::spawner(),
            GitRepo::new(&zcode_host_base::testing::spawner(), &env),
            data.clone(),
            data.join("v2"),
        );
        Self {
            channel: channel(service(checkpoints)),
            home,
            env,
        }
    }

    fn sh(&self, cwd: &Path, args: &[&str]) -> String {
        let output = zcode_proc::std_command("git")
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(self.env.vars())
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repo(&self) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        self.sh(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("old.txt"), "rename me\n").unwrap();
        self.sh(&root, &["add", "-A"]);
        self.sh(&root, &["commit", "-q", "-m", "init"]);
        (dir, root)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        call(&self.channel, method, params)
            .await
            .map_err(|error| error.message)
    }
}

#[tokio::test]
async fn checkpoint_lifecycle_restores_and_detects_conflicts() {
    let fixture = Fixture::new();
    let (_dir, root) = fixture.repo();
    let workspace = root.to_str().unwrap();
    let staged_before = fixture.sh(&root, &["diff", "--cached", "--name-only"]);

    let first = fixture
        .call("createCheckpoint", json!({"workspacePath": workspace}))
        .await
        .unwrap();
    assert_eq!(first["scope"], "workspace");
    assert_eq!(first["workspaceInRepoPath"], ".");
    let manifest = fixture
        .home
        .path()
        .join(".zcode/v2/checkpoints")
        .join(super::checkpoint_store::workspace_hash(workspace))
        .join(format!("{}.json", first["checkpointId"].as_str().unwrap()));
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(text.ends_with("}\n") && text.contains("\n  \"checkpointId\""));
    assert!(
        !fixture
            .home
            .path()
            .join(".zcode/git-checkpoint-index")
            .read_dir()
            .unwrap()
            .any(|_| true)
    );

    // 模拟一轮改动：修改、新增、删除、重命名。
    std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
    std::fs::rename(root.join("old.txt"), root.join("renamed.txt")).unwrap();
    let second = fixture
        .call("createCheckpoint", json!({"workspacePath": workspace}))
        .await
        .unwrap();
    // 用户真实 index 不受影响。
    assert_eq!(
        fixture.sh(&root, &["diff", "--cached", "--name-only"]),
        staged_before
    );

    let pair = json!({
        "workspacePath": workspace,
        "fromCheckpointId": second["checkpointId"],
        "toCheckpointId": first["checkpointId"],
    });
    let diff = fixture.call("diffCheckpoints", pair.clone()).await.unwrap();
    let mut kinds: Vec<String> = diff["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            format!(
                "{}:{}",
                file["kind"].as_str().unwrap(),
                file["repoRelativePath"].as_str().unwrap()
            )
        })
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        ["deleted:new.txt", "modified:a.txt", "renamed:old.txt"]
    );

    // 恢复前用户又改了 a.txt：默认拒绝并报告冲突。
    std::fs::write(root.join("a.txt"), "user edit\n").unwrap();
    let rejected = fixture
        .call("restoreBetweenCheckpoints", pair.clone())
        .await
        .unwrap();
    assert_eq!(rejected["success"], false);
    assert_eq!(rejected["conflicts"][0]["reason"], "content-mismatch");
    assert_eq!(rejected["conflicts"][0]["repoRelativePath"], "a.txt");

    let mut forced = pair.clone();
    forced["force"] = json!(true);
    let restored = fixture
        .call("restoreBetweenCheckpoints", forced)
        .await
        .unwrap();
    assert_eq!(restored["success"], true, "{restored}");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!root.join("new.txt").exists());
    assert!(!root.join("renamed.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("old.txt")).unwrap(),
        "rename me\n"
    );

    let id = first["checkpointId"].clone();
    let deleted = json!({"workspacePath": workspace, "checkpointId": id});
    fixture
        .call("deleteCheckpoint", deleted.clone())
        .await
        .unwrap();
    assert!(!manifest.exists());
    let missing = fixture.call("deleteCheckpoint", deleted).await.unwrap_err();
    assert_eq!(
        missing,
        format!("Checkpoint does not exist: {}", id.as_str().unwrap())
    );
}

#[tokio::test]
async fn checkpoint_requires_repository() {
    let fixture = Fixture::new();
    let plain = tempfile::tempdir().unwrap();
    let error = fixture
        .call(
            "createCheckpoint",
            json!({"workspacePath": plain.path().to_str().unwrap()}),
        )
        .await
        .unwrap_err();
    assert_eq!(error, "Workspace is not inside a Git repository.");
}
