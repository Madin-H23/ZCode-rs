use super::tools::check_cancel;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use zcode_proc::ledger::{Containment, Isolation, Ledger, Purpose};
pub(super) use zcode_proc::tree::Shutdown;
const MAX_STREAM: u64 = 16 * 1024 * 1024;
struct Captured {
    bytes: u64,
    path: PathBuf,
}
#[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
async fn capture(
    mut stream: impl AsyncRead + Unpin,
    path: PathBuf,
    combined: Arc<Mutex<tokio::fs::File>>,
    cancel: CancellationToken,
) -> Result<Captured> {
    let result = async {
        let mut file = tokio::fs::File::create(&path).await?;
        let mut bytes = 0u64;
        let mut buf = [0u8; 8192];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            let write = n.min(MAX_STREAM.saturating_sub(bytes) as usize);
            if write > 0 {
                file.write_all(&buf[..write]).await?;
                combined.lock().await.write_all(&buf[..write]).await?;
            }
            bytes += n as u64;
            if bytes > MAX_STREAM {
                cancel.cancel();
                break;
            }
        }
        file.flush().await?;
        Ok(Captured { bytes, path })
    }
    .await;
    // 输出落盘失败必须停止进程树，不能让已丢失输出的后台任务继续运行。
    if result.is_err() {
        cancel.cancel();
    }
    result
}
/// `(env, processes)`：子进程的完整环境，以及启动进程树时写记录的账本（工具运行时共用）。
#[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
pub(super) async fn run(
    cwd: &Path,
    (env, processes): (&[(String, String)], &Ledger),
    text: &str,
    path: &Path,
    combined: Arc<Mutex<tokio::fs::File>>,
    timeout: Option<Duration>,
    cancel: &CancellationToken,
) -> Result<Value> {
    check_cancel(cancel)?;
    // 修复 B2：Windows 上原来用 args(["/D", "/S", "/C", text])，CRT 引号转义把命令中的 `"` 改写成
    // cmd.exe 不认识的 `\"`；shell::script 用 raw_arg 原样传入（与 hooks 相同）。
    let mut command = zcode_proc::shell::script(text);
    // 子进程只看到启动时计算好的完整环境（净化后加代理/CA），不继承运行时自身的网络变量。
    command
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // 修复 B7：原来用 process_group(0)，子进程留在 runtime 的会话里、保留控制终端，读取 /dev/tty 的
    // ssh / sudo / git 凭据提示会一直挂到超时；Isolation::Session 用 setsid 建新会话（Node detached: true）。
    // Windows 上这棵树在自己的 Job 里：结束时 Job::terminate，根进程退出后留下的后代也一起结束。
    let mut tree = processes
        .spawn(
            &mut command,
            Purpose::Bash,
            Isolation::Session,
            Containment::OwnJob,
        )
        .await
        .context("Cannot start shell")?;
    let overflow = CancellationToken::new();
    let mut out = zcode_async::owned(capture(
        tree.child().stdout.take().unwrap(),
        path.with_extension("stdout"),
        combined.clone(),
        overflow.clone(),
    ));
    let mut err = zcode_async::owned(capture(
        tree.child().stderr.take().unwrap(),
        path.with_extension("stderr"),
        combined.clone(),
        overflow.clone(),
    ));
    let timer = async {
        if let Some(t) = timeout {
            tokio::time::sleep(t).await
        } else {
            std::future::pending::<()>().await
        }
    };
    tokio::pin!(timer);
    let (status, mut reason) = tokio::select! {biased;
        _=cancel.cancelled()=>(None,"cancelled"),
        _=overflow.cancelled()=>(None,"failed"),
        _=&mut timer=>(None,"timed_out"),
        result=tree.child().wait()=>(Some(result?),"completed"),
    };
    let captured = async {
        let shutdown = if status.is_none() {
            Shutdown::Graceful
        } else {
            Shutdown::Immediate
        };
        tree.terminate(shutdown).await?;
        let streams = async { tokio::join!(&mut out, &mut err) };
        tokio::pin!(streams);
        if reason != "completed" {
            // 首次 wait 就被取消时也必须验证管道关闭，不能只在 leader 已退出的分支限时。
            return tokio::time::timeout(Duration::from_secs(1), &mut streams)
                .await
                .context("Bash descendants still hold output after termination");
        }
        // leader exit 不是整个 IO 生命周期结束；仍持有管道的后代不能绕过取消和超时。
        let ready = tokio::select! {biased;
            _=cancel.cancelled(), if reason=="completed"=>{reason="cancelled"; None},
            _=overflow.cancelled(), if reason=="completed"=>{reason="failed"; None},
            _=&mut timer, if reason=="completed"=>{reason="timed_out"; None},
            result=&mut streams=>Some(result),
        };
        if let Some(result) = ready {
            return Ok(result);
        }
        tree.terminate(Shutdown::Graceful).await?;
        // 期限只用于报告无法确认回收的错误，绝不把未关闭的 pipe 当作成功。
        tokio::time::timeout(Duration::from_secs(1), &mut streams)
            .await
            .context("Bash descendants still hold output after termination")
    }
    .await
    .context(crate::contract::ProcessCleanupFailure);
    if captured.is_err() {
        out.abort();
        err.abort();
        return Err(captured.err().unwrap());
    }
    let (out, err) = captured.unwrap();
    let out = out??;
    let err = err??;
    combined.lock().await.flush().await?;
    let reason = if reason == "completed" && !status.as_ref().unwrap().success() {
        "failed"
    } else {
        reason
    };
    // Node posix-bash 把两路输出写入同一文件：模型看合并输出的头部，stderr 字段只放执行器消息。
    let message = match reason {
        "timed_out" => format!(
            "Command timed out after {}",
            super::bash_output::duration(timeout.map_or(0, |t| t.as_millis() as u64))
        ),
        "cancelled" => "Execution cancelled".to_owned(),
        _ if overflow.is_cancelled() => {
            "Execution output exceeded the persisted output limit".to_owned()
        }
        _ => String::new(),
    };
    let output = read_head(path, super::bash_output::INLINE_BYTES).await?;
    let total = out.bytes.min(MAX_STREAM) + err.bytes.min(MAX_STREAM);
    let mut data = json!({"stdout":output,"stderr":message,"status":reason,"interrupted":reason=="cancelled"||reason=="timed_out","timedOut":reason=="timed_out","cancelled":reason=="cancelled","stdoutTruncated":total > super::bash_output::INLINE_BYTES as u64,"stdoutBytes":out.bytes,"stderrBytes":err.bytes,"persistedOutputPath":path,"stdoutPersistedOutputPath":out.path,"stderrPersistedOutputPath":err.path,"persistedOutputSize":total});
    if let Some(code) = status.and_then(|s| s.code()) {
        data["exitCode"] = code.into();
    }
    Ok(data)
}

/// The first `limit` bytes of `path` as text, without a split trailing character.
pub(super) async fn read_head(path: &Path, limit: usize) -> Result<String> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    tokio::fs::File::open(path)
        .await?
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .await?;
    Ok(match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            String::from_utf8_lossy(&error.as_bytes()[..valid]).into_owned()
        }
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    })
}
