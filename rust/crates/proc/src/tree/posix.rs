//! Unix 上结束进程树（docs/specs/rust-u0-foundation.md §4.1）。
//!
//! 按身份（pid + 启动时间）追踪根进程的整棵树：TERM → 宽限 → 循环快照 + KILL，直到快照中没有
//! 存活成员。进程表快照失败时退回对根进程组 killpg，不能一个信号都不发。

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::process::Child;
use tokio::time::Instant;

use super::Shutdown;
use super::table::{self, Table, Tree};

/// TERM 之后等待整棵树自行退出的时间。
const GRACE: Duration = Duration::from_millis(1500);
/// 开始 KILL 之后，确认没有存活成员的期限；超出时报告无法确认回收。
const KILL_DEADLINE: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(10);

/// 结束 `child`（pid 为 `pid`，自成进程组）及其整棵进程树，并回收 `child`。
///
/// `pid` 单独传入：`child` 已被 `wait` 回收后 `Child::id()` 返回 `None`，但残留的后代仍要清理。
/// 返回错误表示无法确认整棵树已结束；调用方据此报告进程清理失败。
pub async fn terminate(child: &mut Child, pid: u32, shutdown: Shutdown) -> Result<()> {
    terminate_with(child, pid, shutdown, &table::snapshot).await
}

/// [`terminate`] 的实现；`snapshot` 提供进程表，测试注入失败的快照。
async fn terminate_with(
    child: &mut Child,
    pid: u32,
    shutdown: Shutdown,
    snapshot: &impl AsyncFn() -> Result<Table>,
) -> Result<()> {
    let root = i32::try_from(pid).context("Invalid process id")?;
    if shutdown == Shutdown::Immediate && !alive(-root) {
        return Ok(());
    }
    let began = Instant::now();
    // 修复 B3：原来 `snapshot().await?` 在 ps 缺失、超时或超出预算时直接返回错误，一个信号都
    // 没有发出，整棵树继续运行。现在退回对根进程组 killpg（spec §4.1）。
    let table = match snapshot().await {
        Ok(table) => table,
        Err(error) => return fall_back(child, root, shutdown, &error).await,
    };
    let mut tree = Tree::new(root, &table);
    if shutdown == Shutdown::Graceful {
        tree.signal(libc::SIGTERM, &table, snapshot).await?;
        while began.elapsed() < GRACE {
            child.try_wait()?;
            if !tree.maybe_live() {
                return Ok(());
            }
            tokio::time::sleep(POLL).await;
        }
    }
    // 单次 killpg 与 fork 竞争会漏掉持有管道的后代。保留组所有权，直到当前快照
    // 确认没有活工作进程；SIGKILL 后的组长退出不是整个工具完成的替代事实。
    let deadline = Instant::now() + KILL_DEADLINE;
    loop {
        let table = match snapshot().await {
            Ok(table) => table,
            // TERM 阶段已经结束（Immediate 本来就跳过它），退回时直接 KILL 根进程组。
            Err(error) => return fall_back(child, root, Shutdown::Immediate, &error).await,
        };
        tree.refresh(&table);
        if !tree.live(&table) {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "Owned Bash processes did not terminate"
        );
        tree.signal(libc::SIGKILL, &table, snapshot).await?;
        child.try_wait()?;
        tokio::time::sleep(POLL).await;
    }
    if child.id().is_some() {
        child.wait().await?;
    }
    Ok(())
}

/// 没有进程表时的退路：只对根进程组发信号，再回收根进程。
///
/// 安全性：只要进程组里还有成员，它的 ID 就不会分配给新进程（Linux、macOS 都如此）；组已不存在时
/// killpg 返回 ESRCH，按已结束处理。已离开根进程组的后代（自建会话或进程组）在这条路径上无法
/// 追踪，这是没有进程表时的已知限制。
async fn fall_back(
    child: &mut Child,
    root: i32,
    shutdown: Shutdown,
    cause: &anyhow::Error,
) -> Result<()> {
    tracing::warn!(
        pid = root,
        error = %format!("{cause:#}"),
        "process table unavailable; signalling the root process group"
    );
    if shutdown == Shutdown::Graceful {
        signal_root(child, root, libc::SIGTERM);
        let began = Instant::now();
        while began.elapsed() < GRACE {
            if settled(child, root)? {
                return Ok(());
            }
            tokio::time::sleep(POLL).await;
        }
    }
    signal_root(child, root, libc::SIGKILL);
    let deadline = Instant::now() + KILL_DEADLINE;
    while !settled(child, root)? {
        ensure!(
            Instant::now() < deadline,
            "Owned Bash processes did not terminate"
        );
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

/// 对根进程组发信号；根进程尚未回收时也直接对它发信号（它的 pid 仍属于本进程，不会被复用），
/// 这样即使根进程不是组长也不会漏掉它。
///
/// 发送失败不立即报错（macOS 上组内有无权发信号的成员时 killpg 返回 EPERM，但其余成员已收到）：
/// 以随后的 [`settled`] 为准，期限内仍未结束时由调用方报告。
fn signal_root(child: &Child, root: i32, signal: i32) {
    let _ = send(-root, signal);
    if child.id().is_some() {
        let _ = send(root, signal);
    }
}

/// 根进程已回收，且根进程组已没有成员。
fn settled(child: &mut Child, root: i32) -> Result<bool> {
    Ok(child.try_wait()?.is_some() && !alive(-root))
}

#[expect(unsafe_code, reason = "kill(2) 没有安全封装")]
pub(super) fn alive(pid: i32) -> bool {
    // SAFETY: kill 只投递信号（0 号信号只做存在与权限检查），不读写本进程内存。
    (unsafe { libc::kill(pid, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[expect(unsafe_code, reason = "kill(2) 没有安全封装")]
pub(super) fn send(pid: i32, signal: i32) -> Result<()> {
    // SAFETY: kill 只投递信号，不读写本进程内存。
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("Cannot signal owned process")
}

#[cfg(test)]
#[path = "posix_tests.rs"]
mod tests;
