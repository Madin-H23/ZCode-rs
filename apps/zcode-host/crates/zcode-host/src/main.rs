//! `zcode-host`：原生 Rust Host 入口（docs/specs/rust-host-r0.md），同时是 CLI 的第二个入口
//! （docs/specs/rust-host-cli-boundary.md §3.4）。
//!
//! 先决定以哪种身份运行，再做任何初始化：
//!
//! 1. 可执行文件名（去掉扩展名）是 `zcode-cli-rust`：作为 CLI 运行，全部参数交给 CLI；
//! 2. 第一个参数是 `cli`：作为 CLI 运行，其余参数交给 CLI；
//! 3. 其余情况作为 Host 运行（Main 以 `--control stdio --allowed-origin …` 启动它）。
//!
//! 作为 CLI 运行时只调用 CLI 库的入口（`zcode_cli_rust::cli::main`），与独立的 `zcode-cli-rust`
//! 完全相同，Host 的运行时、日志与参数都不碰。不含 feature `embedded-runtime` 的构建没有 CLI。
//!
//! 作为 Host 运行时：
//!
//! - stdin / stdout：与 Main 的控制通道，每行一个 JSON；stdout 只放控制消息。
//! - stderr：日志。
//! - `127.0.0.1:<系统分配端口>`：Renderer 的 WebSocket 接入，凭一次性 capability 连接。
//!
//! 本文件是 Host 唯一读取 argv 与环境变量的地方。
mod ws;

use std::ffi::{OsStr, OsString};
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use zcode_host_control::{HostToMain, LogLevel, encode_line, parse_main_to_host};
use zcode_host_core::{ControlOutcome, ControlSink, HostConfig, HostCore};
use zcode_host_kit::log;
use zcode_host_services::HostEnvironment;

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

impl log::LogSink for StdoutControl {
    fn log(&self, level: LogLevel, message: String) {
        let _ = self.0.send(HostToMain::Log {
            level,
            source: "zcode-host",
            message,
        });
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

/// 二进制入口是唯一读取进程环境的地方：之后各服务与进程内 runtime 只用这份快照。
#[expect(
    clippy::disallowed_methods,
    reason = "二进制入口捕获一次 Host 环境（rust-u1-runtime-library L4）"
)]
fn capture_environment() -> Result<HostEnvironment> {
    HostEnvironment::capture(std::env::vars()).context("capture host environment")
}

async fn run(args: Args) -> Result<()> {
    let environment = capture_environment()?;
    // 进程内 runtime 的日志（docs/specs/rust-u3-single-process.md §2.2）；guard 存活到进程退出。
    #[cfg(feature = "embedded-runtime")]
    let _runtime_logs =
        zcode_host_runtime_link::init_logging(&environment.user_home, &environment.vars);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("bind 127.0.0.1")?;
    let port = listener.local_addr()?.port();

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    // 控制写任务由 run 拥有、在 run 返回时结束：它要比 Host 的根作用域活得久，关闭作用域之后
    // （shutdown 的最后一步）仍能写出此前入队的控制消息。
    let _control_writer = zcode_async::owned(write_control(control_rx));
    let core = HostCore::new(
        HostConfig {
            ws_port: port,
            environment,
        },
        Arc::new(StdoutControl(control_tx.clone())),
    );
    let policy = Arc::new(ws::Policy::new(port, args.allowed_origins));
    drop(core.spawner().spawn(
        "websocket-listener",
        ws::serve(listener, core.clone(), policy),
    ));
    let _ = control_tx.send(HostToMain::HostEndpointReady { ws_port: port });
    // 结构化日志经控制通道交给 Main 实时落盘；Main 收到首条结构化日志后不再缓存 stderr。
    log::set_sink(Box::new(StdoutControl(control_tx.clone())));
    log::info(format!("listening on 127.0.0.1:{port}"));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => {}
            Ok(Some(line)) => match parse_main_to_host(&line) {
                Ok(message) => {
                    if core.on_control(message) == ControlOutcome::Shutdown {
                        log::info("dispose received; exiting");
                        core.shutdown().await;
                        return Ok(());
                    }
                }
                Err(error) => log::warn(format!("invalid control line: {error:#}")),
            },
            // Main 退出或关闭控制通道时 Host 随之退出，与 CLI 对 stdin EOF 的处理一致。
            Ok(None) => {
                log::info("control stdin closed; exiting");
                core.shutdown().await;
                return Ok(());
            }
            Err(error) => bail!("control stdin failed: {error}"),
        }
    }
}

/// CLI 的名字：以它为名运行（链接或副本）时整个进程就是 CLI。
const CLI_NAME: &str = "zcode-cli-rust";
/// 把其余参数交给 CLI 的子命令。
const CLI_SUBCOMMAND: &str = "cli";

/// 这次运行的身份（规格 §3.4 的顺序）。
enum Entry {
    /// `program` 是 CLI 看到的程序名，`args` 是交给它的参数。
    Cli {
        program: OsString,
        args: Vec<OsString>,
    },
    Host(Vec<OsString>),
}

fn entry(program: OsString, mut args: Vec<OsString>) -> Entry {
    if Path::new(&program).file_stem() == Some(OsStr::new(CLI_NAME)) {
        return Entry::Cli { program, args };
    }
    if args.first().map(OsString::as_os_str) == Some(OsStr::new(CLI_SUBCOMMAND)) {
        args.remove(0);
        // CLI 的用法文本与 runtime 看到的命令行都以程序名开头：用 CLI 自己的名字，输出与独立 CLI 相同。
        return Entry::Cli {
            program: CLI_NAME.into(),
            args,
        };
    }
    Entry::Host(args)
}

#[cfg(feature = "embedded-runtime")]
fn run_cli(program: OsString, args: Vec<OsString>) -> ExitCode {
    zcode_cli_rust::cli::main(program, args)
}

/// “只有 Host”形态（规格 §3.3）：没有链接 CLI，不能当 CLI 用。
#[cfg(not(feature = "embedded-runtime"))]
fn run_cli(_program: OsString, _args: Vec<OsString>) -> ExitCode {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr().lock(),
        "zcode-host: this build does not include the CLI (built without the embedded-runtime \
         feature); use a zcode-cli-rust executable"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut argv = std::env::args_os();
    let program = argv.next().unwrap_or_default();
    match entry(program, argv.collect()) {
        Entry::Cli { program, args } => run_cli(program, args),
        Entry::Host(args) => host(args),
    }
}

fn host(args: Vec<OsString>) -> ExitCode {
    // 原来 `std::env::args()` 遇到非 Unicode 参数时 panic；现在按参数错误处理（Main 不会传这种参数）。
    let args = match args
        .into_iter()
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(args) => args,
        Err(arg) => {
            log::error(format!(
                "argument is not valid Unicode: {}",
                arg.to_string_lossy()
            ));
            return ExitCode::from(2);
        }
    };
    let args = match parse_args(args.into_iter()) {
        Ok(args) => args,
        Err(error) => {
            log::error(format!("{error:#}"));
            return ExitCode::from(2);
        }
    };
    #[expect(clippy::disallowed_methods, reason = "存量，U3 改为显式传入")]
    log::set_debug(std::env::var("ZCODE_HOST_LOG").is_ok_and(|value| value == "debug"));
    // tokio task 里的 panic 不会结束进程，但必须进 Main 的日志，否则只有 stderr 里能看到。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error(format!("panic: {info}"));
        default_hook(info);
    }));

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
