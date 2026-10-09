//! runtime 链路的选择（docs/specs/rust-host-cli-boundary.md §3.5）。
//!
//! 缺省是进程内的 runtime（feature `embedded`）。只有显式给出 `ZCODE_AGENT_SERVER_COMMAND` 时才启动
//! 子进程：带 `ZCODE_AGENT_SERVER_RUNTIME=zcode-cli-rust` 是 Rust CLI；不带 runtime 时按命令原样启动，
//! 只用作 Host 测试里的协议替身。不含进程内 runtime 的构建（“只有 Host”形态）必须给出命令。
//! Rust Host 不再查找开发态 `zcode.cjs`、TS bundle 或已部署的二进制
//! （rust-unified-host.md D-U9：Rust Host 内只运行 Rust runtime）。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// 怎样到达一个 runtime 实例：选择的结果，也是启动链路的入口（[`RuntimeCommand::start`]）。
///
/// 链路之上的代码只用这里的方法，不按变体分支；两种链路确实不同的事实（pid、stderr、
/// `agent-process-*` 事件）由 [`crate::Link`] 的可选字段表达。
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeCommand {
    /// 进程内的 runtime：作为库运行在 Host 进程里，经内存管道说同一协议。
    #[cfg(feature = "embedded")]
    InProcess(Purpose),
    /// CLI 子进程的 stdio。
    Process(ProcessCommand),
}

/// 子进程链路的命令。
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// 不设置时用 workspace 路径。
    pub cwd: Option<PathBuf>,
    /// 普通 `app-server --stdio` 进程会先发 `startup/storageState`：请求要等存储就绪。
    /// 会话库以同一命令加 `--prepare-storage` 准备（`storagePreparationMode: "process"`）。
    pub supports_storage_startup: bool,
}

/// 进程内实例启动后做什么：子进程链路把它写在参数里（`--prepare-storage`）。
#[cfg(feature = "embedded")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// 服务 App 协议。
    Serve,
    /// 准备会话库（存储握手与迁移）后结束。
    PrepareStorage,
}

/// 进程内 runtime 在日志与生命周期事件里的名字。
#[cfg(feature = "embedded")]
const EMBEDDED_PROGRAM: &str = "zcode-runtime (embedded)";
const RUNTIME_VAR: &str = "ZCODE_AGENT_SERVER_RUNTIME";
const COMMAND_VAR: &str = "ZCODE_AGENT_SERVER_COMMAND";
const RUST_CLI_RUNTIME: &str = "zcode-cli-rust";
const EMBEDDED_RUNTIME: &str = "embedded";

impl RuntimeCommand {
    /// 日志与 `agent-process-*` 事件里的程序名。
    pub fn program(&self) -> String {
        match self {
            #[cfg(feature = "embedded")]
            Self::InProcess(_) => EMBEDDED_PROGRAM.to_owned(),
            Self::Process(command) => command.program.display().to_string(),
        }
    }

    /// 日志与事件里的参数；进程内实例没有。
    pub fn args(&self) -> &[String] {
        match self {
            #[cfg(feature = "embedded")]
            Self::InProcess(_) => &[],
            Self::Process(command) => &command.args,
        }
    }

    /// 命令自带的 cwd；不设置时用 workspace 路径。
    pub fn cwd(&self) -> Option<&Path> {
        match self {
            #[cfg(feature = "embedded")]
            Self::InProcess(_) => None,
            Self::Process(command) => command.cwd.as_deref(),
        }
    }

    /// runtime 是否先发存储启动帧（请求要等存储就绪），以及能否用它准备会话库。
    pub fn supports_storage_startup(&self) -> bool {
        match self {
            #[cfg(feature = "embedded")]
            Self::InProcess(_) => true,
            Self::Process(command) => command.supports_storage_startup,
        }
    }

    /// 在 `cwd` 准备会话库用的同一 runtime：子进程加 `--prepare-storage`（cwd 缺省为 `cwd`），进程内
    /// 实例以准备存储为目的启动。runtime 没有存储握手时为 `None`。
    pub fn for_storage_preparation(&self, cwd: &Path) -> Option<Self> {
        match self {
            #[cfg(feature = "embedded")]
            Self::InProcess(_) => Some(Self::InProcess(Purpose::PrepareStorage)),
            Self::Process(command) if command.supports_storage_startup => {
                let mut prep = command.clone();
                prep.cwd = Some(command.cwd.clone().unwrap_or_else(|| cwd.to_owned()));
                prep.args.push("--prepare-storage".into());
                Some(Self::Process(prep))
            }
            Self::Process(_) => None,
        }
    }
}

fn non_empty<'a>(vars: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    vars.get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

/// `applyPresentationSurfaceToCommand`：去掉已有的 `--surface[=x]`（只吞非选项值），追加 desktop。
fn apply_desktop_surface(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 2);
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--surface" {
            if iter.peek().is_some_and(|next| !next.starts_with('-')) {
                iter.next();
            }
            continue;
        }
        if arg.starts_with("--surface=") {
            continue;
        }
        out.push(arg);
    }
    out.push("--surface".into());
    out.push("desktop".into());
    out
}

/// 没有给出命令时的链路：进程内 runtime；不含它的构建报错（规格 §3.5）。
#[cfg(feature = "embedded")]
fn in_process() -> Result<RuntimeCommand> {
    Ok(RuntimeCommand::InProcess(Purpose::Serve))
}

#[cfg(not(feature = "embedded"))]
fn in_process() -> Result<RuntimeCommand> {
    bail!(
        "this zcode-host build does not include the in-process runtime; set {COMMAND_VAR} to a \
         zcode-cli-rust executable and {RUNTIME_VAR}={RUST_CLI_RUNTIME}"
    )
}

/// 按 Host 环境选择 runtime 链路；规则表见规格 §3.5。
pub fn resolve(vars: &BTreeMap<String, String>, workspace_path: &Path) -> Result<RuntimeCommand> {
    let runtime = non_empty(vars, RUNTIME_VAR);
    let Some(program) = non_empty(vars, COMMAND_VAR) else {
        return match runtime {
            None | Some(EMBEDDED_RUNTIME) => in_process(),
            Some(RUST_CLI_RUNTIME) => {
                bail!("{RUNTIME_VAR}={RUST_CLI_RUNTIME} needs {COMMAND_VAR}")
            }
            Some(_) => bail!("Unsupported {RUNTIME_VAR}"),
        };
    };
    let rust = match runtime {
        Some(RUST_CLI_RUNTIME) => true,
        None => false,
        Some(EMBEDDED_RUNTIME) => bail!("{COMMAND_VAR} is not used by the embedded runtime"),
        Some(_) => bail!("Unsupported {RUNTIME_VAR}"),
    };
    let mut args = match non_empty(vars, "ZCODE_AGENT_SERVER_ARGS_JSON") {
        Some(json) => serde_json::from_str::<Vec<String>>(json)
            .map_err(|_| anyhow::anyhow!("ZCODE_AGENT_SERVER_ARGS_JSON must be a string array"))?,
        None => vec!["app-server".into(), "--stdio".into()],
    };
    if rust {
        if args
            .iter()
            .any(|arg| arg == "--cwd" || arg.starts_with("--cwd="))
        {
            bail!(
                "zcode-cli-rust --cwd is supplied by the Host; remove it from ZCODE_AGENT_SERVER_ARGS_JSON"
            );
        }
        // 显式传 Host 的原始 workspace 路径（OS cwd 会被 realpath 化），保留本地 identity fallback。
        args.push("--cwd".into());
        args.push(workspace_path.display().to_string());
    }
    Ok(RuntimeCommand::Process(ProcessCommand {
        program: PathBuf::from(program),
        args: apply_desktop_surface(args),
        cwd: Some(
            non_empty(vars, "ZCODE_AGENT_SERVER_CWD")
                .map(PathBuf::from)
                .unwrap_or_else(|| workspace_path.to_owned()),
        ),
        supports_storage_startup: rust,
    }))
}

#[cfg(test)]
#[path = "select_tests.rs"]
mod tests;
