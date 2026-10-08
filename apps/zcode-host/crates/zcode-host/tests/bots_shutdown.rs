//! Host 退出时 Bot 通道运行时的收尾（docs/specs/rust-host-bots.md §2.3、§8）：真实 zcode-host 起一个
//! 已配置的 Telegram Bot，长轮询指向 127.0.0.1 上一直挂着不回的 mock；Main 发 `dispose` 后
//! `Installed::shutdown` 必须打断长轮询并立即删除 `telegram-polling` 运行锁，不留给 30 s 租约。
//! HOME 与数据根都是临时目录，控制面基址指向关闭的本地端口，不访问任何真实服务。
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

const TOKEN: &str = "shutdown-test-token";
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// 读一条 HTTP/1.1 请求，返回路径；连接关闭时返回 None。
async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let length: usize = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buffer.len() - header_end - 4;
    while body < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        body += read;
    }
    head.split_whitespace().nth(1).map(str::to_owned)
}

/// Telegram Bot API mock：getUpdates 挂 60 s 不回，其余方法立即回 ok。
async fn telegram_mock(calls: Arc<Mutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let calls = calls.clone();
            tokio::spawn(async move {
                while let Some(path) = read_request(&mut stream).await {
                    let method = path.rsplit('/').next().unwrap_or_default().to_owned();
                    calls.lock().unwrap().push(method.clone());
                    if method == "getUpdates" {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                    let body = json!({"ok": true, "result": true}).to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    origin
}

/// 与 E2E `automation-bots` 相同的预置：一个已绑定的 Telegram Bot，凭据按旧明文值写入。
fn seed(home: &Path) -> PathBuf {
    let config = home.join(".zcode").join("v2");
    std::fs::create_dir_all(&config).unwrap();
    let bots = json!({"version": 3, "bots": [{
        "id": "tg-1", "name": "Bot", "provider": "telegram", "enabled": true,
        "credentialRef": "bot:tg-1:credential", "providerUserId": "4242",
        "allowedWorkspaces": ["*"], "currentOptions": {},
        "allowedCommands": {"status": true, "new": true, "workspace": true, "model": true,
            "mode": true, "thoughtLevel": true, "reply": true},
        "replyMode": "assistant_changes",
    }]});
    std::fs::write(config.join("bot-config.v3.json"), bots.to_string()).unwrap();
    std::fs::write(
        config.join("credentials.json"),
        json!({"bot:tg-1:credential": TOKEN}).to_string(),
    )
    .unwrap();
    config.join("bots-runtime-locks").join("telegram-polling")
}

fn held_locks(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".lock"))
                .count()
        })
        .unwrap_or(0)
}

async fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + STEP_TIMEOUT;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn dispose_releases_bot_runtime_locks_immediately() {
    let home = tempfile::tempdir().unwrap();
    let locks = seed(home.path());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let telegram = telegram_mock(calls.clone()).await;
    let mut child = Command::new(env!("CARGO_BIN_EXE_zcode-host"))
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ZCODE_DESKTOP_HOME_DIR", home.path())
        .env("ZCODE_DATA_BASE_DIR", home.path())
        .env("ZCODE_CREDENTIAL_SECRET", "bots-shutdown-test")
        .env("ZCODE_BASE_URL", "http://127.0.0.1:9")
        .env("ZCODE_TELEGRAM_API_BASE_URL", &telegram)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn zcode-host");
    let mut stdin = child.stdin.take().unwrap();
    // 一直读控制输出（日志也走 stdout），避免管道写满阻塞 Host；记下启动阶段。
    let ready = Arc::new(Mutex::new(false));
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready_flag = ready.clone();
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["type"] == "database-startup-state" && message["state"]["phase"] == "ready" {
                *ready_flag.lock().unwrap() = true;
            }
        }
    });
    let init = json!({"type": "init-local", "databaseStartupId": "startup-1",
        "zcodeBuiltinProviderConfigFilePath": "/x"});
    stdin
        .write_all(format!("{init}\n").as_bytes())
        .await
        .unwrap();
    until("host ready", || *ready.lock().unwrap()).await;
    until("Telegram long poll in flight", || {
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "getUpdates")
    })
    .await;
    assert_eq!(held_locks(&locks), 1, "polling holds the runtime lock");

    let started = Instant::now();
    stdin
        .write_all(format!("{}\n", json!({"type": "dispose"})).as_bytes())
        .await
        .unwrap();
    let status = tokio::time::timeout(STEP_TIMEOUT, child.wait())
        .await
        .expect("host exits after dispose")
        .unwrap();
    assert!(status.success());
    // Main 在 dispose 3.5 s 后强杀 Host：Bot 收尾不能吃掉 CLI 回收的时间。
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "shutdown took {:?}",
        started.elapsed()
    );
    assert_eq!(
        held_locks(&locks),
        0,
        "runtime lock is released on shutdown"
    );
}
