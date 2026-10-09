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
    process::Child,
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
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
#[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
pub(super) async fn run(
    cwd: &Path,
    env: &[(String, String)],
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
    // ssh / sudo / git 凭据提示会一直挂到超时；isolate 用 setsid 建新会话（Node detached: true）。
    zcode_proc::group::isolate(&mut command);
    let mut child = command.spawn().context("Cannot start shell")?;
    let pid = child.id().context("Missing child pid")?;
    let tree = ProcessTree::contain(&child, pid);
    let overflow = CancellationToken::new();
    let mut out = tokio::spawn(capture(
        child.stdout.take().unwrap(),
        path.with_extension("stdout"),
        combined.clone(),
        overflow.clone(),
    ));
    let mut err = tokio::spawn(capture(
        child.stderr.take().unwrap(),
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
        result=child.wait()=>(Some(result?),"completed"),
    };
    let captured = async {
        let shutdown = if status.is_none() {
            Shutdown::Graceful
        } else {
            Shutdown::Immediate
        };
        tree.terminate(&mut child, shutdown).await?;
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
        tree.terminate(&mut child, Shutdown::Graceful).await?;
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

/// 一棵子进程树的所有权与结束方式。
///
/// Unix：按身份追踪整棵树（`zcode_proc::tree`），根进程须自成进程组。Windows：Bash 工具的子进程
/// 在 spawn 后立即加入 Job，结束时 `Job::terminate`，Job 被 drop 时其中残留的进程也一起结束；
/// 没有 Job（MCP、hooks，或加入 Job 失败）时退回 `taskkill /T /F`。
pub(super) struct ProcessTree {
    pid: u32,
    #[cfg(windows)]
    job: Option<zcode_proc::job::Job>,
}

impl ProcessTree {
    /// 不加入 Job 的子进程（MCP stdio、hooks）：Windows 上用 taskkill 结束。
    pub(super) fn uncontained(pid: u32) -> Self {
        Self {
            pid,
            #[cfg(windows)]
            job: None,
        }
    }

    /// Bash 工具的子进程：Windows 上紧接 spawn 加入 Job，加入失败时退回 taskkill。
    pub(super) fn contain(child: &Child, pid: u32) -> Self {
        #[cfg(windows)]
        {
            let job = zcode_proc::job::Job::new().and_then(|job| job.assign(child).map(|()| job));
            match job {
                Ok(job) => Self {
                    pid,
                    job: Some(job),
                },
                Err(error) => {
                    tracing::warn!(
                        pid,
                        %error,
                        "cannot contain the process in a Job; falling back to taskkill"
                    );
                    Self::uncontained(pid)
                }
            }
        }
        #[cfg(unix)]
        {
            let _ = child;
            Self::uncontained(pid)
        }
    }

    /// 结束整棵树并回收 `child`；错误表示无法确认回收。Windows 上没有 TERM 阶段，`shutdown` 不起作用。
    pub(super) async fn terminate(&self, child: &mut Child, shutdown: Shutdown) -> Result<()> {
        #[cfg(unix)]
        {
            zcode_proc::tree::terminate(child, self.pid, shutdown).await
        }
        #[cfg(windows)]
        {
            let _ = shutdown;
            if let Some(job) = &self.job {
                // 根进程退出后 taskkill /T 已无法沿父进程找到后代；Job 不依赖父子关系，整棵树一起结束。
                job.terminate()
                    .context("Cannot terminate the process job")?;
                if child.try_wait()?.is_none() {
                    child.wait().await?;
                }
                return Ok(());
            }
            let result = zcode_proc::command("taskkill")
                .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status()
                .await;
            if child.try_wait()?.is_none() {
                anyhow::ensure!(result?.success(), "Bash taskkill failed");
                child.wait().await?;
            }
            Ok(())
        }
    }
}
