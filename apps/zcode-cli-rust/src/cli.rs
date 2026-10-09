//! The CLI's entry (spec rust-host-cli-boundary §3.4): what `zcode-cli-rust
//! <args>` does — argument parsing, the `-p` headless run, `app-server
//! --stdio`, the `tui` placeholder, signals, logging and exit codes.
//!
//! The standalone binary and the merged `zcode-host` binary (its `cli`
//! subcommand, or run under the CLI's name) both only call [`main`]. It is the
//! one place that reads the process environment and cwd; the command line is
//! the one it is given, never the process's own.
mod args;
mod prompt;

use std::ffi::OsString;
use std::process::ExitCode;

use anyhow::Result;
use args::{AppServerArgs, Cli, Command};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use zcode_cli_app_server::{Sink, stdio};

use crate::runtime::RuntimeOptions;
use crate::serve::AppServerOptions;

/// Runs the CLI on its own tokio runtime. `program` is the name it was invoked
/// under (clap's usage text shows it; the runtime's view of the command line
/// starts with it) and `args` everything after it. Returns when `app-server`
/// ends normally; every other outcome exits the process from inside, as the
/// standalone binary always has.
pub fn main(program: OsString, args: Vec<OsString>) -> ExitCode {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed building the Runtime")
        .block_on(run(program, args));
    ExitCode::SUCCESS
}

async fn run(program: OsString, args: Vec<OsString>) {
    let lossy = |arg: &OsString| arg.to_string_lossy().into_owned();
    let argv: Vec<String> = args.iter().map(lossy).collect();
    let runtime_argv: Vec<String> = std::iter::once(&program).chain(&args).map(lossy).collect();
    // 子命令走 clap；其余与 Node 一样由顶层参数决定（-p 无头运行或 TUI）。
    if !matches!(argv.first().map(String::as_str), Some("app-server" | "tui")) {
        std::process::exit(prompt::main(&argv, runtime_argv).await);
    }
    let cli = Cli::parse_from(std::iter::once(program).chain(args));
    if let Err(error) = subcommand(cli, runtime_argv).await {
        // stderr 断管也不能递归进入错误处理；stdout 永远只用于协议。
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "zcode-cli-rust: {error}");
        std::process::exit(1);
    }
}

async fn subcommand(cli: Cli, argv: Vec<String>) -> Result<()> {
    match cli.command {
        Command::AppServer(args) => app_server(args, argv).await,
        Command::Tui => tui_unavailable(),
    }
}

/// TUI 入口先占位：保留子命令与退出码契约，避免未实现的前端伪装成可用。
fn tui_unavailable() -> ! {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr().lock(),
        "zcode-cli-rust: TUI 尚未实现，请使用 app-server --stdio 或 -p"
    );
    std::process::exit(zcode_cli_tui::UNAVAILABLE_EXIT_CODE);
}

async fn app_server(args: AppServerArgs, argv: Vec<String>) -> Result<()> {
    // B4 缺陷修复：HOME 为空时旧的 home() 返回空路径，日志与数据会写到当前工作目录下。
    // 现在解析失败就让启动失败（run 把错误写到 stderr 并以 1 退出）。
    let home = zcode_kit::paths::home_dir()?;
    #[expect(
        clippy::disallowed_methods,
        reason = "CLI 入口读取进程环境（rust-host-cli-boundary §3.4）"
    )]
    let log_options =
        zcode_cli_host::logging::LogOptions::from_env(&home, |name| std::env::var_os(name));
    let log_dir = log_options.directory.clone();
    let _logs = zcode_cli_host::logging::init(log_options);
    // 实例的任务作用域：在日志之后创建、之前释放，本函数返回时其中剩余的任务被中止。
    let scope = crate::runtime::instance_scope();
    let tasks = scope.spawner();
    zcode_cli_host::log_retention::schedule(&tasks, log_dir);
    tracing::info!(
        target: "zcode::runtime",
        event = "runtime.started",
        version = env!("CARGO_PKG_VERSION"),
        surface = args.surface.as_str(),
        prepare_storage = args.prepare_storage,
        "App server starting"
    );
    // 进程环境与 cwd 只在这里读取一次；库内其余位置只用这份选项。
    let options = AppServerOptions {
        runtime: RuntimeOptions::from_process(home, argv, args.cwd, args.data_dir)?,
        config: args.config,
        desktop: args.surface == "desktop",
        prepare_storage: args.prepare_storage,
    };
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tasks.spawn("signals", async move {
        #[cfg(unix)]
        {
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signal_cancel.cancel();
    });
    let input = stdio::start(CancellationToken::new());
    let (output, writer) = Sink::stdout(cancel.clone());
    let served = crate::serve::app_server(options, &tasks, input, output, cancel).await;
    // 失败路径同样先等写线程落盘：存储失败的错误响应必须送达 Host 后进程才能退出。
    let drained = stdio::finish(writer).await;
    served.and(drained)
}
