mod args;
mod prompt;
use anyhow::Result;
use args::{AppServerArgs, Cli, Command};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use zcode_cli_app_server::{Sink, stdio};
use zcode_cli_rust::runtime::RuntimeOptions;
use zcode_cli_rust::serve::AppServerOptions;

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // 子命令走 clap；其余与 Node 一样由顶层参数决定（-p 无头运行或 TUI）。
    if !matches!(argv.first().map(String::as_str), Some("app-server" | "tui")) {
        std::process::exit(prompt::main(&argv).await);
    }
    if let Err(error) = run().await {
        // stderr 断管也不能递归进入错误处理；stdout 永远只用于协议。
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "zcode-cli-rust: {error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    match Cli::parse().command {
        Command::AppServer(args) => app_server(args).await,
        Command::Tui => tui_unavailable(),
    }
}

/// TUI 入口先占位：保留子命令与退出码契约，避免未实现的前端伪装成可用。
pub(crate) fn tui_unavailable() -> ! {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr().lock(),
        "zcode-cli-rust: TUI 尚未实现，请使用 app-server --stdio 或 -p"
    );
    std::process::exit(zcode_cli_tui::UNAVAILABLE_EXIT_CODE);
}

async fn app_server(args: AppServerArgs) -> Result<()> {
    // B4 缺陷修复：HOME 为空时旧的 home() 返回空路径，日志与数据会写到当前工作目录下。
    // 现在解析失败就让启动失败（main 把错误写到 stderr 并以 1 退出）。
    let home = zcode_kit::paths::home_dir()?;
    let log_options = zcode_cli_host::logging::LogOptions::from_env(&home);
    let log_dir = log_options.directory.clone();
    let _logs = zcode_cli_host::logging::init(log_options);
    zcode_cli_host::log_retention::schedule(log_dir);
    tracing::info!(
        target: "zcode::runtime",
        event = "runtime.started",
        version = env!("CARGO_PKG_VERSION"),
        surface = args.surface.as_str(),
        prepare_storage = args.prepare_storage,
        "App server starting"
    );
    // 进程环境、argv 与 cwd 只在这里读取一次；库内其余位置只用这份选项。
    let options = AppServerOptions {
        runtime: RuntimeOptions::from_process(home, args.cwd, args.data_dir)?,
        config: args.config,
        desktop: args.surface == "desktop",
        prepare_storage: args.prepare_storage,
    };
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
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
    let served = zcode_cli_rust::serve::app_server(options, input, output, cancel).await;
    // 失败路径同样先等写线程落盘：存储失败的错误响应必须送达 Host 后进程才能退出。
    let drained = stdio::finish(writer).await;
    served.and(drained)
}
