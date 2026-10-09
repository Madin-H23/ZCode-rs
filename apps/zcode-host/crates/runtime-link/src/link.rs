//! 一条到 runtime 的协议链路（docs/specs/rust-host-cli-boundary.md §3.2）：子进程的 stdio，或进程内
//! runtime 的内存管道。两者说同一份 App 协议；链路之上的代码只用这里的类型，不区分传输。
use std::collections::BTreeMap;
use std::path::Path;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, ChildStderr};

use crate::process::{self, Spawned};
use crate::select::RuntimeCommand;

/// 启动一个 runtime 实例所需的事实；`env` 就是子进程链路传给 CLI 的那份环境。
pub struct Start<'a> {
    /// Host 提交的原始 workspace 路径（身份用它，不做 realpath）。
    pub workspace: &'a Path,
    pub identity: Option<&'a str>,
    /// 子进程的 cwd；进程内实例解析相对路径的基准。
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, String>,
}

/// 一条已建立的链路。
pub struct Link {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// 子进程才有；进程内实例没有 pid，也没有 stderr（它的日志走 tracing）。
    pub pid: Option<u32>,
    pub stderr: Option<ChildStderr>,
    pub exit: Exit,
}

impl From<Spawned> for Link {
    fn from(spawned: Spawned) -> Self {
        Self {
            reader: Box::new(spawned.stdout),
            writer: Box::new(spawned.stdin),
            pid: spawned.pid,
            stderr: Some(spawned.stderr),
            exit: Exit(Lifetime::Process {
                child: spawned.child,
                pid: spawned.pid,
            }),
        }
    }
}

impl RuntimeCommand {
    /// 建立链路：启动子进程，或在当前 tokio 运行时上启动进程内实例。
    pub fn start(&self, start: Start<'_>) -> std::io::Result<Link> {
        match self {
            #[cfg(feature = "embedded")]
            Self::InProcess(purpose) => crate::embedded::start(start, *purpose),
            Self::Process(command) => {
                process::spawn(&command.program, &command.args, start.cwd, start.env)
                    .map(Link::from)
            }
        }
    }
}

/// runtime 结束时的事实。
#[derive(Clone)]
pub struct Outcome {
    /// 进入日志与关闭原因的文本。
    pub reason: String,
    pub success: bool,
    /// 子进程才有退出状态。
    pub status: Option<std::process::ExitStatus>,
}

/// runtime 的生命期句柄：等它结束、回收它。
pub struct Exit(Lifetime);

enum Lifetime {
    Process {
        child: Child,
        pid: Option<u32>,
    },
    #[cfg(feature = "embedded")]
    InProcess(crate::embedded::Running),
}

impl Exit {
    /// 等 runtime 结束。
    pub async fn wait(&mut self) -> Outcome {
        match &mut self.0 {
            Lifetime::Process { child, .. } => match child.wait().await {
                Ok(status) => Outcome {
                    reason: format!("process_exit: {status}"),
                    success: status.success(),
                    status: Some(status),
                },
                Err(error) => Outcome {
                    reason: format!("process_exit: {error}"),
                    success: false,
                    status: None,
                },
            },
            #[cfg(feature = "embedded")]
            Lifetime::InProcess(running) => running.wait().await,
        }
    }

    /// 回收：子进程按 [`process::terminate`] 的顺序升级信号；进程内实例先取消，再等任务结束。
    /// 子进程的调用方应已关闭它的 stdin。
    pub async fn terminate(&mut self) {
        match &mut self.0 {
            Lifetime::Process { child, pid } => process::terminate(child, *pid).await,
            #[cfg(feature = "embedded")]
            Lifetime::InProcess(running) => running.terminate().await,
        }
    }

    /// 已结束的子进程的退出状态；仍在运行或进程内实例为 None。
    pub fn exit_status(&mut self) -> Option<std::process::ExitStatus> {
        match &mut self.0 {
            Lifetime::Process { child, .. } => child.try_wait().ok().flatten(),
            #[cfg(feature = "embedded")]
            Lifetime::InProcess(_) => None,
        }
    }
}

impl Exit {
    #[cfg(feature = "embedded")]
    pub(crate) fn in_process(running: crate::embedded::Running) -> Self {
        Self(Lifetime::InProcess(running))
    }
}
