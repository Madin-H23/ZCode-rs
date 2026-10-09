use super::*;
use serde_json::json;
use std::time::Instant;

fn request(program: Program, timeout_ms: u64, limit: usize) -> HookProcess {
    HookProcess {
        program,
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        env: vec![("ZCODE_SESSION_ID".into(), "s-1".into())],
        input: json!({"hookEventName":"UserPromptSubmit","prompt":"hi","sessionId":"s-1"}),
        timeout: Duration::from_millis(timeout_ms),
        max_output_bytes: limit,
    }
}

fn shell(command: &str) -> Program {
    Program::Command {
        command: command.into(),
        shell: Shell::Unset,
        background: false,
    }
}

#[expect(clippy::disallowed_methods, reason = "测试子进程沿用进程的 PATH")]
fn base_env() -> Vec<(String, String)> {
    vec![("PATH".into(), std::env::var("PATH").unwrap_or_default())]
}

/// 运行 hook；无论结果如何，结束后账本里都不剩记录（docs/specs/rust-tool-process-reaping.md 2.1）。
async fn run_hook(request: HookProcess, cancel: &CancellationToken) -> Exec {
    let dir = tempfile::tempdir().unwrap();
    let processes = Ledger::open(dir.path().join("processes"));
    let result = run(request, (&base_env(), &processes), cancel)
        .await
        .unwrap();
    let left: Vec<_> = std::fs::read_dir(dir.path().join("processes"))
        .map(|entries| entries.map(|entry| entry.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert_eq!(left, Vec::<std::ffi::OsString>::new(), "{}", result.status);
    result
}

#[tokio::test]
async fn runs_with_stdin_env_and_transcript() {
    let script = r#"read line; printf '%s|%s|' "$line" "$ZCODE_SESSION_ID"; p=$(printf '%s' "$line" | sed 's/.*"transcript_path":"\([^"]*\)".*/\1/'); cat "$p"; echo "$p" >&2; exit 3"#;
    let result = run_hook(
        request(shell(script), 5000, 4096),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(result.status, "failed");
    assert_eq!(result.exit_code, Some(3));
    let (line, rest) = result.stdout.split_once('|').unwrap();
    let stdin: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(stdin["hook_event_name"], "UserPromptSubmit");
    assert!(rest.starts_with("s-1|{\"message\":{\"content\":[{\"text\":\"hi\""));
    // 运行结束后临时目录已删除。
    let transcript = result.stderr.trim();
    assert!(transcript.ends_with("transcript.jsonl"));
    assert!(!std::path::Path::new(transcript).exists());
}

#[tokio::test]
async fn caps_each_stream_without_killing() {
    let script = "head -c 100000 /dev/zero | tr '\\0' a; head -c 10 /dev/zero | tr '\\0' b >&2";
    let result = run_hook(request(shell(script), 5000, 64), &CancellationToken::new()).await;
    assert_eq!(result.status, "completed");
    assert_eq!(result.stdout, "a".repeat(64));
    assert_eq!(result.stderr, "b".repeat(10));
}

#[tokio::test]
async fn times_out_and_cancels_the_tree() {
    let started = Instant::now();
    let result = run_hook(
        request(shell("sleep 30"), 200, 64),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(result.status, "timed_out");
    assert!(started.elapsed() < Duration::from_secs(5));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let result = run_hook(request(shell("sleep 30"), 30_000, 64), &cancel).await;
    assert_eq!(result.status, "cancelled");
}

#[tokio::test]
async fn reclaims_descendants_holding_output() {
    let started = Instant::now();
    let result = run_hook(
        request(shell("sleep 30 & echo done"), 30_000, 64),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(result.status, "completed");
    assert_eq!(result.stdout, "done\n");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn reports_start_failures_like_node() {
    let program = Program::Process {
        command: "/nonexistent/hook".into(),
        args: vec![],
    };
    let result = run_hook(request(program, 1000, 64), &CancellationToken::new()).await;
    assert_eq!(result.status, "spawn_error");
    assert_eq!(
        result.error.as_deref(),
        Some("spawn /nonexistent/hook ENOENT")
    );
}
