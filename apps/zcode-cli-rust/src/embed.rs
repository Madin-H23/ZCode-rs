//! Runs the runtime inside the caller's process (spec rust-u1-runtime-library §3).
//!
//! The embedder gets one end of an in-memory pipe that speaks exactly what the
//! binary speaks on stdio: request lines in, replies and frames out. Closing the
//! write half is stdin EOF; the runtime then drains like the process does.
//!
//! Every task of an instance belongs to the instance's scope
//! (spec rust-u2-task-scope §4.1), which lives exactly as long as the instance
//! task: it is dropped when the task returns, fails, panics or is aborted, and
//! whatever is still running then is aborted. The instance task itself and the
//! embedding process's log retention are the runtime's only direct
//! `tokio::spawn` calls; both go away when the embedder passes its own scope.
use crate::serve::{AppServerOptions, app_server};
use anyhow::Result;
use tokio_util::sync::CancellationToken;
use zcode_cli_app_server::{Sink, stdio};

/// Bytes the pipe buffers in each direction before the writer waits. Output the
/// embedder has not read yet counts as the sink's backlog, as on stdout.
const PIPE_BYTES: usize = 64 * 1024;

pub struct Embedded {
    /// The embedder's end: write request lines, read replies and frames (NDJSON).
    pub io: tokio::io::DuplexStream,
    /// Completes when the runtime has stopped; an error is its reason.
    pub task: tokio::task::JoinHandle<Result<()>>,
    /// Asks the runtime to drain and stop (what SIGTERM does to the process).
    pub cancel: CancellationToken,
    /// What of this instance is still running.
    pub tasks: Tasks,
}

/// A read-only view of an instance's scope, for the embedder's shutdown log and
/// tests: after the instance task has ended, nothing of the instance runs for
/// long (blocking work cannot be aborted and finishes on its own).
#[derive(Clone)]
pub struct Tasks(zcode_async::Spawner);

impl Tasks {
    /// Tasks still running, as `<scope path>: <task name>`.
    pub fn running(&self) -> Vec<String> {
        self.0.running()
    }
}

/// Starts a runtime and its App Server on the current tokio runtime.
pub fn spawn(options: AppServerOptions) -> Embedded {
    let scope = crate::runtime::instance_scope();
    let tasks = scope.spawner();
    let (io, runtime_io) = tokio::io::duplex(PIPE_BYTES);
    let (reader, writer) = tokio::io::split(runtime_io);
    let cancel = CancellationToken::new();
    let input = stdio::start_reader(
        tokio::io::BufReader::new(reader),
        CancellationToken::new(),
        &tasks,
    );
    let (output, writing) = Sink::writer(writer, cancel.clone());
    let (serving, instance) = (cancel.clone(), tasks.clone());
    // Host 需要 `JoinHandle<anyhow::Result<()>>`：实例任务本身仍用 tokio::spawn，作用域移进
    // 任务，随任务结束（含 panic、被中止）而释放。Host 传入自己的作用域之后去掉。
    let task = tokio::spawn(async move {
        let _scope = scope;
        let served = app_server(options, &instance, input, output, serving).await;
        // 与二进制相同：先等剩余输出写完（或确认对端已关闭），再报告结束。
        let drained = stdio::finish_task(writing).await;
        served.and(drained)
    });
    Embedded {
        io,
        task,
        cancel,
        tasks: Tasks(tasks),
    }
}

/// Log retention of a process that embeds the runtime (the binary schedules it
/// in its instance scope instead).
pub mod log_retention {
    pub use zcode_cli_host::log_retention::{Cleanup, RETENTION_DAYS, STARTUP_DELAY, cleanup};

    /// Runs [`cleanup`] once after [`STARTUP_DELAY`] in the background of the
    /// embedding process.
    pub fn schedule(dir: std::path::PathBuf) {
        // 进程级任务，归嵌入方进程所有；Host 传入自己的作用域之后改为在其中启动。
        tokio::spawn(zcode_cli_host::log_retention::delayed(dir));
    }
}
