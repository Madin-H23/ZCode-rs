//! `zcode-host`：原生 Rust Host 入口（docs/specs/rust-host-r0.md）。
//!
//! - stdin / stdout：与 Main 的控制通道，每行一个 JSON；stdout 只放控制消息。
//! - stderr：日志。
//! - `127.0.0.1:<系统分配端口>`：Renderer 的 WebSocket 接入，凭一次性 capability 连接。
//!
//! 本文件是唯一读取 argv 与环境变量的地方。
mod ws;

use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use zcode_host_control::{HostToMain, encode_line, parse_main_to_host};
use zcode_host_core::{ControlOutcome, ControlSink, HostConfig, HostCore, log};

struct Args {
    allowed_origins: Vec<String>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args> {
    let mut allowed_origins = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--allowed-origin" => {
                allowed_origins.push(args.next().context("--allowed-origin requires a value")?)
            }
            "--control" => {
                let value = args.next().context("--control requires a value")?;
                if value != "stdio" {
                    bail!("unsupported --control {value}; only stdio is available");
                }
            }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(Args { allowed_origins })
}

/// 控制消息只入队，由单独的写任务按顺序写入 stdout。
struct StdoutControl(mpsc::UnboundedSender<HostToMain>);

impl ControlSink for StdoutControl {
    fn send(&self, message: HostToMain) {
        let _ = self.0.send(message);
    }
}

async fn write_control(mut messages: mpsc::UnboundedReceiver<HostToMain>) {
    let mut stdout = tokio::io::stdout();
    while let Some(message) = messages.recv().await {
        let line = encode_line(&message);
        if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
            log::error("control stdout closed");
            return;
        }
    }
}

async fn run(args: Args) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("bind 127.0.0.1")?;
    let port = listener.local_addr()?.port();

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    tokio::spawn(write_control(control_rx));
    let core = HostCore::new(
        HostConfig { ws_port: port },
        Arc::new(StdoutControl(control_tx.clone())),
    );
    let policy = Arc::new(ws::Policy::new(port, args.allowed_origins));
    tokio::spawn(ws::serve(listener, core.clone(), policy));
    let _ = control_tx.send(HostToMain::HostEndpointReady { ws_port: port });
    log::info(format!("listening on 127.0.0.1:{port}"));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => {}
            Ok(Some(line)) => match parse_main_to_host(&line) {
                Ok(message) => {
                    if core.on_control(message) == ControlOutcome::Shutdown {
                        log::info("dispose received; exiting");
                        return Ok(());
                    }
                }
                Err(error) => log::warn(format!("invalid control line: {error:#}")),
            },
            // Main 退出或关闭控制通道时 Host 随之退出，与 CLI 对 stdin EOF 的处理一致。
            Ok(None) => {
                log::info("control stdin closed; exiting");
                return Ok(());
            }
            Err(error) => bail!("control stdin failed: {error}"),
        }
    }
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            log::error(format!("{error:#}"));
            return ExitCode::from(2);
        }
    };
    log::set_debug(std::env::var("ZCODE_HOST_LOG").is_ok_and(|value| value == "debug"));

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error(format!("tokio runtime: {error}"));
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(args));
    // stdin 的阻塞读线程不会因为 Dispose 返回；不等待它，直接结束进程。
    runtime.shutdown_background();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error(format!("{error:#}"));
            ExitCode::FAILURE
        }
    }
}
