//! Runs the runtime inside the caller's process (spec rust-u1-runtime-library §3).
//!
//! The embedder gets one end of an in-memory pipe that speaks exactly what the
//! binary speaks on stdio: request lines in, replies and frames out. Closing the
//! write half is stdin EOF; the runtime then drains like the process does.
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
}

/// Starts a runtime and its App Server on the current tokio runtime.
pub fn spawn(options: AppServerOptions) -> Embedded {
    let (io, runtime_io) = tokio::io::duplex(PIPE_BYTES);
    let (reader, writer) = tokio::io::split(runtime_io);
    let cancel = CancellationToken::new();
    let input = stdio::start_reader(tokio::io::BufReader::new(reader), CancellationToken::new());
    let (output, writing) = Sink::writer(writer, cancel.clone());
    let serving = cancel.clone();
    let task = tokio::spawn(async move {
        let served = app_server(options, input, output, serving).await;
        // 与二进制相同：先等剩余输出写完（或确认对端已关闭），再报告结束。
        let drained = stdio::finish_task(writing).await;
        served.and(drained)
    });
    Embedded { io, task, cancel }
}
