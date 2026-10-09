//! Top-level entry without a subcommand (spec rust-m4-headless): `-p` runs one
//! prompt through the headless frontend against an in-process engine.
use crate::runtime;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use zcode_cli_core_api::ClientMsg;
use zcode_cli_headless::{
    self as headless,
    args::{self, ArgError, Parsed, PromptArgs},
};

/// Node `DEFAULT_CLI_CLEANUP_TIMEOUT_MS` / `DEFAULT_SHUTDOWN_CLEANUP_TIMEOUT_MS`.
const CLEANUP: Duration = Duration::from_secs(6);
const SIGNAL_CLEANUP: Duration = Duration::from_secs(2);

async fn write(stderr: bool, text: &str) {
    if stderr {
        let mut out = tokio::io::stderr();
        let _ = out.write_all(text.as_bytes()).await;
        let _ = out.flush().await;
    } else {
        let mut out = tokio::io::stdout();
        let _ = out.write_all(text.as_bytes()).await;
        let _ = out.flush().await;
    }
}

async fn usage(error: ArgError) -> i32 {
    let mut text = format!("{}\n", error.message);
    if error.help {
        text.push('\n');
        text.push_str(&headless::help(env!("CARGO_PKG_VERSION")));
    }
    write(true, &text).await;
    1
}

/// Node `path.resolve`: absolute and lexically normalized.
fn resolve(base: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in base.join(path).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Node `resolveCliCwd`.
#[expect(
    clippy::disallowed_methods,
    reason = "CLI 入口读取进程 cwd（rust-host-cli-boundary §3.4）"
)]
async fn working_directory(requested: Option<&str>) -> Result<PathBuf, String> {
    let base = std::env::current_dir().map_err(|e| e.to_string())?;
    let Some(requested) = requested else {
        return Ok(base);
    };
    if requested.is_empty() {
        return Err("--cwd requires a non-empty path.".into());
    }
    let directory = resolve(&base, requested);
    match tokio::fs::metadata(&directory).await {
        Err(_) => Err(format!(
            "--cwd path is not accessible: {}",
            directory.display()
        )),
        Ok(meta) if !meta.is_dir() => Err(format!(
            "--cwd must point to a directory: {}",
            directory.display()
        )),
        Ok(_) => Ok(directory),
    }
}

/// `argv` is everything after the program name; `runtime_argv` is the whole
/// command line the runtime sees (spec rust-host-cli-boundary §3.4).
pub(super) async fn main(argv: &[String], runtime_argv: Vec<String>) -> i32 {
    let prompt = match args::parse(argv) {
        Ok(Parsed::Prompt(prompt)) => *prompt,
        Ok(Parsed::Help) => {
            write(false, &headless::help(env!("CARGO_PKG_VERSION"))).await;
            return 0;
        }
        Ok(Parsed::Version) => {
            write(false, &format!("{}\n", env!("CARGO_PKG_VERSION"))).await;
            return 0;
        }
        Ok(Parsed::Tui) => super::tui_unavailable(),
        Err(error) => return usage(error).await,
    };
    let cwd = match working_directory(prompt.cwd.as_deref()).await {
        Ok(cwd) => cwd,
        Err(message) => {
            write(true, &format!("{message}\n")).await;
            return 1;
        }
    };
    if let Err(error) = args::check_prompt(&prompt.prompt) {
        return usage(error).await;
    }
    match run(prompt, cwd, runtime_argv).await {
        Ok(code) => code,
        Err(error) => {
            write(true, &format!("Error: {error}\n")).await;
            1
        }
    }
}

/// `--attach` paths as V4 attachment refs; missing files are dropped silently
/// (Node replaces them with a placeholder the model never sees).
async fn attachments(paths: &[String], cwd: &Path) -> Vec<Value> {
    let mut refs = vec![];
    for path in paths {
        let absolute = resolve(cwd, path);
        let is_file = tokio::fs::metadata(&absolute)
            .await
            .is_ok_and(|m| m.is_file());
        let name = absolute
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        if let (true, Some(name)) = (is_file, name) {
            let location = absolute.to_string_lossy().into_owned();
            refs.push(json!({"ref": location, "fileName": name,
                "mime": headless::attachment_mime(&location), "bytes": 0}));
        }
    }
    refs
}

/// First signal: interrupt the run; second: exit at once (Node `once` handlers).
fn watch_signals(tasks: &zcode_async::Spawner, interrupt: watch::Sender<Option<i32>>) {
    tasks.spawn("signals", async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut int), Ok(mut term), Ok(mut hup)) = (
                signal(SignalKind::interrupt()),
                signal(SignalKind::terminate()),
                signal(SignalKind::hangup()),
            ) else {
                return;
            };
            loop {
                let name = tokio::select! {
                    _ = int.recv() => "SIGINT",
                    _ = term.recv() => "SIGTERM",
                    _ = hup.recv() => "SIGHUP",
                };
                let code = headless::signal_exit_code(name);
                if interrupt.borrow().is_some() {
                    std::process::exit(code);
                }
                let _ = interrupt.send(Some(code));
            }
        }
        #[cfg(not(unix))]
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            let code = headless::signal_exit_code("SIGINT");
            if interrupt.borrow().is_some() {
                std::process::exit(code);
            }
            let _ = interrupt.send(Some(code));
        }
    });
}

async fn run(prompt: PromptArgs, cwd: PathBuf, argv: Vec<String>) -> anyhow::Result<i32> {
    // B4 缺陷修复：同 app-server，home 解析失败时报错退出，不再把数据写到当前工作目录下。
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
    let scope = runtime::instance_scope();
    let tasks = scope.spawner();
    zcode_cli_host::log_retention::schedule(&tasks, log_dir);
    tracing::info!(
        target: "zcode::runtime",
        event = "runtime.started",
        version = env!("CARGO_PKG_VERSION"),
        surface = prompt.surface.as_str(),
        mode = prompt.mode.as_str(),
        "Headless prompt starting"
    );
    let (interrupt_tx, interrupt) = watch::channel(None);
    watch_signals(&tasks, interrupt_tx);
    let data_dir = prompt.data_dir.as_ref().map(PathBuf::from);
    let options = runtime::RuntimeOptions::from_process(home, argv, Some(cwd.clone()), data_dir)?;
    let ctx = runtime::Context::prepare(options, tasks.clone()).await?;
    let store: std::sync::Arc<dyn zcode_cli_core_api::SessionStore> =
        std::sync::Arc::new(ctx.node_store().await?);
    let config = prompt.config.as_ref().map(PathBuf::from);
    let engine = ctx
        .engine(store, config.as_ref(), prompt.surface == "desktop")
        .await?
        .with_headless();
    let path = ctx.requested_cwd.to_string_lossy().into_owned();
    let mut workspace = json!({"workspacePath": path, "workspaceKey": ctx.workspace});
    if ctx.workspace != path {
        workspace["workspaceIdentity"] = ctx.workspace.clone().into();
    }
    let request = headless::Request {
        attachments: attachments(&prompt.attach, &ctx.cwd).await,
        prompt: prompt.prompt,
        format: prompt.format,
        mode: prompt.mode,
        resume: prompt.resume,
        disallowed_tools: prompt.disallowed_tools,
        workspace,
        directory: path,
        verbose: prompt.verbose,
    };
    let (to, input) = mpsc::channel(64);
    let (output, mut from) = mpsc::channel(64);
    let actor = zcode_async::owned(engine.serve(input, output, CancellationToken::new()));
    let mut io = headless::Io {
        stdout: tokio::io::stdout(),
        stderr: tokio::io::stderr(),
    };
    let code = headless::run(request, (&to, &mut from), &mut io, interrupt.clone()).await;
    // Eof：Engine 取消运行与后台任务并落盘后结束；清理有上限（Node closeApp / 信号清理）。
    let _ = to.send(ClientMsg::Eof).await;
    let limit = if interrupt.borrow().is_some() {
        SIGNAL_CLEANUP
    } else {
        CLEANUP
    };
    let drained = async {
        while from.recv().await.is_some() {}
        let _ = actor.await;
    };
    if tokio::time::timeout(limit, drained).await.is_err() {
        tracing::warn!(
            target: "zcode::runtime",
            event = "runtime.cleanup_timeout",
            "Headless cleanup exceeded its limit"
        );
    }
    Ok(code)
}
