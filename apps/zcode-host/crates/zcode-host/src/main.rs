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
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use zcode_host_control::{HostToMain, LogLevel, encode_line, parse_main_to_host};
use zcode_host_core::{ControlOutcome, ControlSink, HostConfig, HostCore};
use zcode_host_gateway::OutboundLimits;
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

/// 控制消息队列的容量（条，日志不计在内）。
///
/// 发送方是同步的 [`ControlSink::send`]（Gateway 的连接循环、各 service 的任意线程），不能等待；
/// 控制消息又多是 Main 在等的应答，不能悄悄丢。正常情况下每秒只有几条，写任务写满管道时才积压：
/// 积压到这个数说明 Main 已经很久不读 stdout（挂起），继续缓存只会让 Host 的内存无界增长。此时丢弃
/// 新消息并计数，写任务恢复时补一条 error 日志说明丢了多少（docs/specs/rust-u2-task-scope.md §6.2）。
const CONTROL_QUEUE: usize = 65_536;
/// 结构化日志队列的容量（条）。日志是诊断信息：满了丢弃新日志并计数，恢复时补一条 warn。
const LOG_QUEUE: usize = 8_192;

/// 满了而被丢弃的消息数；写任务在下一次写出时报告并清零。
#[derive(Default)]
struct Dropped {
    control: AtomicU64,
    logs: AtomicU64,
}

/// 控制消息与日志只入队，由单独的写任务按顺序写入 stdout（控制消息优先）。两条有界队列，满了的
/// 策略见 [`CONTROL_QUEUE`] 与 [`LOG_QUEUE`]。
#[derive(Clone)]
struct StdoutControl {
    control: mpsc::Sender<HostToMain>,
    logs: mpsc::Sender<HostToMain>,
    dropped: Arc<Dropped>,
}

impl ControlSink for StdoutControl {
    fn send(&self, message: HostToMain) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.control.try_send(message) {
            self.dropped.control.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl log::LogSink for StdoutControl {
    fn log(&self, level: LogLevel, message: String) {
        let log = HostToMain::Log {
            level,
            source: "zcode-host",
            message,
        };
        if let Err(mpsc::error::TrySendError::Full(_)) = self.logs.try_send(log) {
            self.dropped.logs.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn write_line(stdout: &mut tokio::io::Stdout, message: &HostToMain) -> bool {
    let line = encode_line(message);
    stdout.write_all(line.as_bytes()).await.is_ok() && stdout.flush().await.is_ok()
}

/// 等写任务写完此前入队的全部消息的请求（Host 退出前）。
type Flush = tokio::sync::oneshot::Sender<()>;

/// Host 退出前等写任务写完已入队消息的时限：Main 不读 stdout 时不让退出一直等。
const FLUSH_ON_EXIT: std::time::Duration = std::time::Duration::from_secs(2);

async fn write_control(
    mut control: mpsc::Receiver<HostToMain>,
    mut logs: mpsc::Receiver<HostToMain>,
    mut flushes: mpsc::Receiver<Flush>,
    dropped: Arc<Dropped>,
) {
    let mut stdout = tokio::io::stdout();
    loop {
        let message = tokio::select! {
            biased;
            Some(message) = control.recv() => message,
            Some(message) = logs.recv() => message,
            // 两条队列都空了才轮到它（biased）：此前入队的消息都已写出。
            Some(flush) = flushes.recv() => {
                let _ = flush.send(());
                continue;
            }
            else => return,
        };
        if !write_line(&mut stdout, &message).await {
            log::error("control stdout closed");
            return;
        }
        for (count, level, what) in [
            (&dropped.control, LogLevel::Error, "control messages"),
            (&dropped.logs, LogLevel::Warn, "log lines"),
        ] {
            let lost = count.swap(0, Ordering::Relaxed);
            if lost > 0 {
                let report = HostToMain::Log {
                    level,
                    source: "zcode-host",
                    message: format!(
                        "control stdout fell behind; dropped {lost} {what} while Main was not reading"
                    ),
                };
                if !write_line(&mut stdout, &report).await {
                    return;
                }
            }
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

/// 测试用的出站上限（docs/specs/rust-u2-gateway.md §2.6）：只在调试构建中编译，发布构建忽略这个变量。
#[cfg(debug_assertions)]
const DEBUG_OUTBOUND_HARD_LIMIT: &str = "ZCODE_HOST_DEBUG_OUTBOUND_HARD_LIMIT";

/// 出站队列阈值，以及要在日志通道就绪后记下的说明（覆盖了上限，或变量的值无效）。
#[cfg(debug_assertions)]
fn outbound_limits(environment: &HostEnvironment) -> (OutboundLimits, Option<String>) {
    let mut limits = OutboundLimits::default();
    let Some(value) = environment.vars.get(DEBUG_OUTBOUND_HARD_LIMIT) else {
        return (limits, None);
    };
    match value.trim().parse::<usize>() {
        Ok(bytes) if bytes > 0 => {
            limits.hard_limit = bytes;
            let note = format!(
                "{DEBUG_OUTBOUND_HARD_LIMIT}={bytes} (debug build): connections close above {bytes} queued bytes"
            );
            (limits, Some(note))
        }
        _ => (
            limits,
            Some(format!(
                "ignoring {DEBUG_OUTBOUND_HARD_LIMIT}={value:?}: expected a positive byte count"
            )),
        ),
    }
}

#[cfg(not(debug_assertions))]
fn outbound_limits(_environment: &HostEnvironment) -> (OutboundLimits, Option<String>) {
    (OutboundLimits::default(), None)
}

async fn run(args: Args) -> Result<()> {
    let environment = capture_environment()?;
    let (outbound_limits, outbound_note) = outbound_limits(&environment);
    // 进程内 runtime 的日志（docs/specs/rust-u3-single-process.md §2.2）；guard 存活到进程退出。
    #[cfg(feature = "embedded-runtime")]
    let _runtime_logs =
        zcode_host_runtime_link::init_logging(&environment.user_home, &environment.vars);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("bind 127.0.0.1")?;
    let port = listener.local_addr()?.port();

    let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE);
    let (log_tx, log_rx) = mpsc::channel(LOG_QUEUE);
    let (flush_tx, flush_rx) = mpsc::channel(1);
    let control = StdoutControl {
        control: control_tx,
        logs: log_tx,
        dropped: Arc::default(),
    };
    // 控制写任务由 run 拥有、在 run 返回时结束：它要比 Host 的根作用域活得久，关闭作用域之后
    // （shutdown 的最后一步）仍能写出此前入队的控制消息。
    let _control_writer = zcode_async::owned(write_control(
        control_rx,
        log_rx,
        flush_rx,
        control.dropped.clone(),
    ));
    // 缺陷修复：run 返回时写任务随句柄被中止，关闭作用域之后才入队的消息（关闭报告等最后几条日志）
    // 可能还没写出就丢了。现在退出前先等写任务把已入队的消息写完（有上限）。
    let flush = async move || {
        let (done, flushed) = tokio::sync::oneshot::channel();
        if flush_tx.send(done).await.is_ok() {
            let _ = tokio::time::timeout(FLUSH_ON_EXIT, flushed).await;
        }
    };
    let core = HostCore::new(
        HostConfig {
            ws_port: port,
            environment,
            outbound_limits,
        },
        Arc::new(control.clone()),
    );
    let policy = Arc::new(ws::Policy::new(port, args.allowed_origins));
    // 监听是常驻任务：Host 退出关闭根作用域时随取消信号结束。
    drop(core.spawner().spawn_resident(
        "websocket-listener",
        ws::serve(listener, core.clone(), policy),
    ));
    control.send(HostToMain::HostEndpointReady { ws_port: port });
    // 结构化日志经控制通道交给 Main 实时落盘；Main 收到首条结构化日志后不再缓存 stderr。
    log::set_sink(Box::new(control.clone()));
    log::info(format!("listening on 127.0.0.1:{port}"));
    if let Some(note) = outbound_note {
        log::warn(note);
    }

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => {}
            Ok(Some(line)) => match parse_main_to_host(&line) {
                Ok(message) => {
                    if core.on_control(message) == ControlOutcome::Shutdown {
                        log::info("dispose received; exiting");
                        drop(core.shutdown().await);
                        flush().await;
                        return Ok(());
                    }
                }
                Err(error) => log::warn(format!("invalid control line: {error:#}")),
            },
            // Main 退出或关闭控制通道时 Host 随之退出，与 CLI 对 stdin EOF 的处理一致。
            Ok(None) => {
                log::info("control stdin closed; exiting");
                drop(core.shutdown().await);
                flush().await;
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
