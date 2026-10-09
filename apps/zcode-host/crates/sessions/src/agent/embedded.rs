//! 进程内 runtime 的链路（docs/specs/rust-u3-single-process.md §2.2）。
//!
//! runtime 作为库运行在 Host 进程里，经内存管道说与 stdio 相同的协议；实例的环境、路径都由
//! Host 显式给出，runtime 不读 Host 进程的环境与 cwd。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use zcode_cli_rust::{embed, runtime::RuntimeOptions, serve::AppServerOptions};

use super::process::{Exit, Link, Outcome};

/// 取消后等 runtime 自行收口（取消运行、回收工具进程树、提交中断状态）的时间；超出后中止任务。
/// 与子进程链路“等 1.8 s → TERM → 等 2 s”的总预算相当。
const DRAIN_WAIT: Duration = Duration::from_millis(3800);

/// 启动一个 runtime 实例所需的事实；`env` 就是子进程链路传给 CLI 的那份环境。
pub struct Start<'a> {
    /// Host 提交的原始 workspace 路径（身份用它，不做 realpath）。
    pub workspace: &'a Path,
    pub identity: Option<&'a str>,
    /// 相对路径的解析基准：子进程链路里 CLI 的 cwd。
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    pub prepare_storage: bool,
}

pub fn start(start: Start<'_>) -> std::io::Result<Link> {
    let value = |key: &str| start.env.get(key).filter(|value| !value.is_empty());
    let home = value("HOME")
        .or_else(|| value("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the runtime environment has no home directory",
            )
        })?;
    let data_dir = value("ZCODE_CLI_RUST_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".zcode/rust"));
    let embedded = embed::spawn(AppServerOptions {
        runtime: RuntimeOptions {
            home,
            env: start
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            argv: vec!["zcode-host".into(), "app-server".into(), "--stdio".into()],
            cwd: start.workspace.to_owned(),
            workspace_identity: start.identity.map(str::to_owned),
            base_dir: start.cwd.to_owned(),
            data_dir,
        },
        config: None,
        desktop: true,
        prepare_storage: start.prepare_storage,
    });
    let (reader, writer) = tokio::io::split(embedded.io);
    Ok(Link {
        reader: Box::new(reader),
        writer: Box::new(writer),
        pid: None,
        stderr: None,
        exit: Exit::Embedded(Running {
            task: Some(embedded.task),
            cancel: embedded.cancel,
            outcome: None,
        }),
    })
}

/// 进程内 runtime 用 `tracing` 记日志：由 Host 进程安装它的 JSONL subscriber（写同一个
/// `zcode-rust-<date>.jsonl`），并安排一次过期日志清理。目录与级别取自 Host 的环境，规则与 CLI
/// 进程相同。返回的 guard 在进程退出前保持存活；已有 subscriber 时返回 `None`。
pub fn init_logging(
    home: &Path,
    vars: &BTreeMap<String, String>,
) -> Option<zcode_cli_rust::logging::LogGuard> {
    let options =
        zcode_cli_rust::logging::LogOptions::from_env(home, |name| vars.get(name).map(Into::into));
    let directory = options.directory.clone();
    let guard = zcode_cli_rust::logging::init(options)?;
    zcode_cli_rust::log_retention::schedule(directory);
    Some(guard)
}

/// 一个运行中的嵌入式 runtime 实例。
pub struct Running {
    task: Option<JoinHandle<anyhow::Result<()>>>,
    cancel: CancellationToken,
    outcome: Option<Outcome>,
}

impl Running {
    /// 等实例结束；结束事实只计算一次，之后重复返回。
    pub async fn wait(&mut self) -> Outcome {
        if let Some(task) = self.task.as_mut() {
            let joined = task.await;
            self.finish(joined);
        }
        self.outcome.clone().unwrap_or_else(|| Outcome {
            reason: "runtime_exit: unknown".into(),
            success: false,
            status: None,
        })
    }

    /// 请求实例收口并等它结束；超过 [`DRAIN_WAIT`] 仍未结束时中止任务。
    pub async fn terminate(&mut self) {
        self.cancel.cancel();
        let Some(task) = self.task.as_mut() else {
            return;
        };
        let joined = match tokio::time::timeout(DRAIN_WAIT, &mut *task).await {
            Ok(joined) => joined,
            Err(_) => {
                task.abort();
                task.await
            }
        };
        self.finish(joined);
    }

    fn finish(&mut self, joined: Result<anyhow::Result<()>, JoinError>) {
        self.task = None;
        // 异常结束（错误、panic、中止）时实例内仍可能有派生任务在等这个信号；正常结束时无影响。
        self.cancel.cancel();
        self.outcome = Some(match joined {
            Ok(Ok(())) => Outcome {
                reason: "runtime_exit: ok".into(),
                success: true,
                status: None,
            },
            Ok(Err(error)) => Outcome {
                reason: format!("runtime_exit: {error:#}"),
                success: false,
                status: None,
            },
            Err(error) if error.is_panic() => {
                let payload = error.into_panic();
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_owned());
                Outcome {
                    reason: format!("runtime_panicked: {message}"),
                    success: false,
                    status: None,
                }
            }
            Err(_) => Outcome {
                reason: "runtime_aborted".into(),
                success: false,
                status: None,
            },
        });
    }
}
