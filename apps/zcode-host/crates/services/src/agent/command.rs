//! CLI 命令解析（zcodeAgentProcessManager.ts:355-519）。
//!
//! 优先级：显式环境变量覆盖 → monorepo 开发态 `zcode.cjs` → 指定的 bundle → 已部署原生二进制。
//! Rust Host 自身不是 Node：运行 `zcode.cjs` 需要 Main 提供 Electron 可执行文件
//! （`ZCODE_HOST_NODE_EXEC_PATH`，以 `ELECTRON_RUN_AS_NODE=1` 当作 Node 用）。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub struct AgentCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// 不设置时用 workspace 路径。
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// 普通 `app-server --stdio` 进程会先发 `startup/storageState`：请求要等存储就绪。
    pub supports_storage_startup: bool,
}

fn non_empty<'a>(vars: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    vars.get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

fn find_upward(start: &Path, relative: &str) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(relative))
        .find(|candidate| candidate.is_file())
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

fn app_server_args() -> Vec<String> {
    vec!["app-server".into(), "--stdio".into()]
}

pub fn resolve(
    vars: &BTreeMap<String, String>,
    search_roots: &[PathBuf],
    workspace_path: &Path,
) -> Result<Option<AgentCommand>> {
    let command = if let Some(program) = non_empty(vars, "ZCODE_AGENT_SERVER_COMMAND") {
        let runtime = non_empty(vars, "ZCODE_AGENT_SERVER_RUNTIME");
        if runtime.is_some_and(|runtime| runtime != "zcode-cli-rust") {
            bail!("Unsupported ZCODE_AGENT_SERVER_RUNTIME");
        }
        let mut args = match non_empty(vars, "ZCODE_AGENT_SERVER_ARGS_JSON") {
            Some(json) => serde_json::from_str::<Vec<String>>(json).map_err(|_| {
                anyhow::anyhow!("ZCODE_AGENT_SERVER_ARGS_JSON must be a string array")
            })?,
            None => app_server_args(),
        };
        let rust = runtime == Some("zcode-cli-rust");
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
        Some(AgentCommand {
            program: PathBuf::from(program),
            args,
            cwd: Some(
                non_empty(vars, "ZCODE_AGENT_SERVER_CWD")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| workspace_path.to_owned()),
            ),
            env: BTreeMap::new(),
            supports_storage_startup: rust,
        })
    } else {
        let node = non_empty(vars, "ZCODE_HOST_NODE_EXEC_PATH").map(PathBuf::from);
        let bytecode = non_empty(vars, "ZCODE_DESKTOP_AGENT_BYTECODE") == Some("1");
        let dist = if bytecode {
            "apps/zcode-cli/packages/cli/dist/zcode.bytecode.cjs"
        } else {
            "apps/zcode-cli/packages/cli/dist/zcode.cjs"
        };
        let bundle = non_empty(vars, "ZCODE_AGENT_BUNDLE_PATH")
            .map(PathBuf::from)
            .or_else(|| search_roots.iter().find_map(|root| find_upward(root, dist)));
        match (node, bundle) {
            (Some(node), Some(bundle)) => {
                let mut args = vec![bundle.display().to_string()];
                args.extend(app_server_args());
                Some(AgentCommand {
                    program: node,
                    args,
                    cwd: None,
                    // 以纯 Node 模式运行 Electron，否则子进程会被当成 Chromium 进程卡在 GPU 初始化。
                    env: BTreeMap::from([("ELECTRON_RUN_AS_NODE".to_owned(), "1".to_owned())]),
                    supports_storage_startup: true,
                })
            }
            _ => non_empty(vars, "GLM_BINARY_PATH").map(|binary| AgentCommand {
                program: PathBuf::from(binary),
                args: app_server_args(),
                cwd: None,
                env: BTreeMap::new(),
                supports_storage_startup: false,
            }),
        }
    };
    Ok(command.map(|mut command| {
        command.args = apply_desktop_surface(command.args);
        command
    }))
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

    #[test]
    fn rust_cli_override_appends_cwd_and_surface() {
        let command = resolve(
            &vars(&[
                ("ZCODE_AGENT_SERVER_COMMAND", "/bin/zcode-cli-rust"),
                ("ZCODE_AGENT_SERVER_RUNTIME", "zcode-cli-rust"),
            ]),
            &[],
            Path::new("/w"),
        )
        .unwrap()
        .unwrap();
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
    fn rejects_cwd_in_rust_args_and_unknown_runtime() {
        assert!(
            resolve(
                &vars(&[
                    ("ZCODE_AGENT_SERVER_COMMAND", "x"),
                    ("ZCODE_AGENT_SERVER_RUNTIME", "zcode-cli-rust"),
                    (
                        "ZCODE_AGENT_SERVER_ARGS_JSON",
                        r#"["app-server","--cwd","/x"]"#
                    ),
                ]),
                &[],
                Path::new("/w"),
            )
            .is_err()
        );
        assert!(
            resolve(
                &vars(&[
                    ("ZCODE_AGENT_SERVER_COMMAND", "x"),
                    ("ZCODE_AGENT_SERVER_RUNTIME", "node")
                ]),
                &[],
                Path::new("/w"),
            )
            .is_err()
        );
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

    #[test]
    fn bundle_needs_node_executable() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir
            .path()
            .join("apps/zcode-cli/packages/cli/dist/zcode.cjs");
        std::fs::create_dir_all(bundle.parent().unwrap()).unwrap();
        std::fs::write(&bundle, "").unwrap();
        let roots = [dir.path().join("packages/desktop")];
        assert_eq!(resolve(&vars(&[]), &roots, Path::new("/w")).unwrap(), None);
        let command = resolve(
            &vars(&[("ZCODE_HOST_NODE_EXEC_PATH", "/Electron")]),
            &roots,
            Path::new("/w"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(command.program, PathBuf::from("/Electron"));
        assert_eq!(command.args[0], bundle.display().to_string());
        assert_eq!(command.env["ELECTRON_RUN_AS_NODE"], "1");
    }
}
