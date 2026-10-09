//! runtime 链路的选择（docs/specs/rust-u3-single-process.md §2.2）。
//!
//! 缺省是进程内的嵌入式 runtime。只有显式给出 `ZCODE_AGENT_SERVER_COMMAND` 时才启动子进程：带
//! `ZCODE_AGENT_SERVER_RUNTIME=zcode-cli-rust` 是 Rust CLI（差分对照）；不带 runtime 时按命令原样启动，
//! 只用作 Host 测试里的协议替身。Rust Host 不再查找开发态 `zcode.cjs`、TS bundle 或已部署的二进制
//! （rust-unified-host.md D-U9：Rust Host 内只运行 Rust runtime）。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub struct AgentCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// 不设置时用 workspace 路径。
    pub cwd: Option<PathBuf>,
    /// 普通 `app-server --stdio` 进程会先发 `startup/storageState`：请求要等存储就绪。
    /// 会话库以同一命令加 `--prepare-storage` 准备（`storagePreparationMode: "process"`）。
    pub supports_storage_startup: bool,
    /// runtime 作为库运行在 Host 进程内（docs/specs/rust-u3-single-process.md §2.2）：不启动
    /// 子进程，`program` 与 `args` 只用于日志。
    pub embedded: bool,
}

/// 嵌入式 runtime 在日志与生命周期事件里的名字。
const EMBEDDED_PROGRAM: &str = "zcode-runtime (embedded)";
const RUNTIME_VAR: &str = "ZCODE_AGENT_SERVER_RUNTIME";
const COMMAND_VAR: &str = "ZCODE_AGENT_SERVER_COMMAND";
const RUST_CLI_RUNTIME: &str = "zcode-cli-rust";
const EMBEDDED_RUNTIME: &str = "embedded";

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

fn embedded() -> AgentCommand {
    AgentCommand {
        program: PathBuf::from(EMBEDDED_PROGRAM),
        args: Vec::new(),
        cwd: None,
        supports_storage_startup: true,
        embedded: true,
    }
}

/// 按 Host 环境选择 runtime 链路；规则表见规格 §2.2“选择方式”。
pub fn resolve(vars: &BTreeMap<String, String>, workspace_path: &Path) -> Result<AgentCommand> {
    let runtime = non_empty(vars, RUNTIME_VAR);
    let Some(program) = non_empty(vars, COMMAND_VAR) else {
        return match runtime {
            None | Some(EMBEDDED_RUNTIME) => Ok(embedded()),
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
    Ok(AgentCommand {
        program: PathBuf::from(program),
        args: apply_desktop_surface(args),
        cwd: Some(
            non_empty(vars, "ZCODE_AGENT_SERVER_CWD")
                .map(PathBuf::from)
                .unwrap_or_else(|| workspace_path.to_owned()),
        ),
        supports_storage_startup: rust,
        embedded: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn resolve_with(pairs: &[(&str, &str)]) -> Result<AgentCommand> {
        resolve(&vars(pairs), Path::new("/w"))
    }

    #[test]
    fn embedded_is_the_default_and_the_explicit_choice() {
        for pairs in [
            &[][..],
            &[(RUNTIME_VAR, "embedded")][..],
            &[(RUNTIME_VAR, " "), (COMMAND_VAR, "")][..],
        ] {
            let command = resolve_with(pairs).unwrap();
            assert!(command.embedded, "{pairs:?}");
            assert!(command.supports_storage_startup);
            assert_eq!(command.cwd, None);
        }
    }

    #[test]
    fn the_implicit_discovery_chain_is_gone() {
        // Main 给 Node Host 准备的变量（以及开发态 bundle 的查找输入）不再选出子进程链路。
        let command = resolve_with(&[
            ("GLM_BINARY_PATH", "/bundle/zcode"),
            ("ZCODE_HOST_NODE_EXEC_PATH", "/Electron"),
            ("ZCODE_AGENT_BUNDLE_PATH", "/bundle/zcode.cjs"),
            ("ZCODE_DESKTOP_AGENT_BYTECODE", "1"),
        ])
        .unwrap();
        assert_eq!(command, embedded());
    }

    #[test]
    fn rust_cli_override_appends_cwd_and_surface() {
        let command = resolve_with(&[
            (COMMAND_VAR, "/bin/zcode-cli-rust"),
            (RUNTIME_VAR, "zcode-cli-rust"),
        ])
        .unwrap();
        assert!(!command.embedded);
        assert_eq!(command.program, PathBuf::from("/bin/zcode-cli-rust"));
        assert_eq!(
            command.args,
            [
                "app-server",
                "--stdio",
                "--cwd",
                "/w",
                "--surface",
                "desktop"
            ]
        );
        assert!(command.supports_storage_startup);
        assert_eq!(command.cwd.as_deref(), Some(Path::new("/w")));
    }

    #[test]
    fn a_command_without_a_runtime_is_a_protocol_double() {
        let command = resolve_with(&[
            (COMMAND_VAR, "/tmp/fake-cli"),
            ("ZCODE_AGENT_SERVER_CWD", "/elsewhere"),
        ])
        .unwrap();
        assert!(!command.embedded);
        assert!(!command.supports_storage_startup);
        assert_eq!(
            command.args,
            ["app-server", "--stdio", "--surface", "desktop"]
        );
        assert_eq!(command.cwd.as_deref(), Some(Path::new("/elsewhere")));
    }

    #[test]
    fn inconsistent_selections_are_rejected() {
        for pairs in [
            // 子进程链路必须给出命令，不能静默退回嵌入式。
            &[(RUNTIME_VAR, "zcode-cli-rust")][..],
            &[(RUNTIME_VAR, "node")][..],
            &[(COMMAND_VAR, "x"), (RUNTIME_VAR, "embedded")][..],
            &[(COMMAND_VAR, "x"), (RUNTIME_VAR, "node")][..],
            &[
                (COMMAND_VAR, "x"),
                (RUNTIME_VAR, "zcode-cli-rust"),
                (
                    "ZCODE_AGENT_SERVER_ARGS_JSON",
                    r#"["app-server","--cwd","/x"]"#,
                ),
            ][..],
            &[(COMMAND_VAR, "x"), ("ZCODE_AGENT_SERVER_ARGS_JSON", "{}")][..],
        ] {
            assert!(resolve_with(pairs).is_err(), "{pairs:?}");
        }
    }

    #[test]
    fn surface_flag_is_replaced() {
        assert_eq!(
            apply_desktop_surface(vec![
                "a".into(),
                "--surface".into(),
                "terminal".into(),
                "--stdio".into()
            ]),
            ["a", "--stdio", "--surface", "desktop"]
        );
        assert_eq!(
            apply_desktop_surface(vec!["--surface".into(), "--stdio".into()]),
            ["--stdio", "--surface", "desktop"]
        );
    }
}
