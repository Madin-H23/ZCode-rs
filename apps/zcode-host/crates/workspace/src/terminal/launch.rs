//! 终端启动参数（terminalService.ts resolveTerminalShell / resolveTerminalCwd / resolveTerminalEnv）。
//!
//! shell 不带任何参数（不是登录 shell）：登录 shell 的 PATH 等已由 Main 的环境补丁进入基础环境。
//! 设置里的 `integratedTerminalShell` 只给 Agent 的 Bash 工具用，这里不读。
use std::path::Path;

use zcode_host_kit::RpcError;

use crate::support::env::ProcessEnv;

/// 从 Dock/Finder 启动时 PATH 往往只有系统目录：只在终端环境里补常见 Homebrew 与系统路径。
const DARWIN_FALLBACK_PATHS: [&str; 8] = [
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

/// `access(X_OK)`：Windows 上只检查存在（与 Node 一致）。
async fn is_executable(path: &Path) -> bool {
    match tokio::fs::metadata(path).await {
        #[cfg(unix)]
        Ok(metadata) => {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        Ok(_) => true,
        Err(_) => false,
    }
}

/// 含分隔符的候选直接检查；裸名逐个 PATH 目录拼接检查（不处理 PATHEXT，与 TS 相同）。
async fn is_usable_command(command: &str, env: &ProcessEnv) -> bool {
    if command.contains(['/', '\\']) {
        return is_executable(Path::new(command)).await;
    }
    for dir in env.path_dirs() {
        if !dir.is_empty() && is_executable(&Path::new(&dir).join(command)).await {
            return true;
        }
    }
    false
}

pub async fn resolve_shell(env: &ProcessEnv) -> Result<String, RpcError> {
    let (candidates, error) = if cfg!(windows) {
        // Windows PowerShell 5.1 的 PSReadLine 在 ConPTY 下重绘更容易出错，优先 PowerShell 7+。
        (
            vec![
                Some("pwsh.exe"),
                Some("powershell.exe"),
                env.get("ComSpec"),
                Some("cmd.exe"),
            ],
            "No usable Windows shell found for terminal startup",
        )
    } else {
        // SHELL 可能残留一个已不存在的路径：先校验，不可用再按常见 shell 回退。
        (
            vec![
                env.get("SHELL"),
                Some("/bin/zsh"),
                Some("/bin/bash"),
                Some("/bin/sh"),
            ],
            "No usable shell found for terminal startup",
        )
    };
    for candidate in candidates.into_iter().flatten() {
        if !candidate.is_empty() && is_usable_command(candidate, env).await {
            return Ok(candidate.to_owned());
        }
    }
    Err(RpcError::new("Error", error))
}

/// 传入目录失效时静默回退到 home 或根目录，保证终端还能拉起。
pub async fn resolve_cwd(requested: Option<&str>, env: &ProcessEnv) -> Result<String, RpcError> {
    let candidates = [
        requested,
        env.get("HOME"),
        env.get("USERPROFILE"),
        Some("/"),
    ];
    for candidate in candidates.into_iter().flatten() {
        if !candidate.is_empty()
            && tokio::fs::metadata(candidate)
                .await
                .is_ok_and(|metadata| metadata.is_dir())
        {
            return Ok(candidate.to_owned());
        }
    }
    Err(RpcError::new(
        "Error",
        "No usable working directory found for terminal startup",
    ))
}

fn is_utf8_locale(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("utf8") || lower.contains("utf-8")
}

fn is_missing_or_c_locale(value: Option<&str>) -> bool {
    matches!(
        value
            .map(|value| value.trim().to_ascii_uppercase())
            .as_deref(),
        None | Some("" | "C" | "POSIX")
    )
}

fn merge_path_entries(entries: &[&str], delimiter: char) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut merged: Vec<&str> = Vec::new();
    for value in entries {
        for entry in value.split(delimiter).map(str::trim) {
            if !entry.is_empty() && seen.insert(entry) {
                merged.push(entry);
            }
        }
    }
    merged.join(&delimiter.to_string())
}

/// `resolveTerminalEnv`：交互终端需要的 TERM / COLORTERM / UTF-8 locale。
pub fn terminal_env(base: &ProcessEnv, macos: bool) -> ProcessEnv {
    let mut env = base.clone();
    let fallback_locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|key| base.get(key))
        .find(|value| is_utf8_locale(value))
        .map(str::to_owned)
        .unwrap_or_else(|| if macos { "en_US.UTF-8" } else { "C.UTF-8" }.to_owned());
    if macos {
        let mut entries = vec![base.get("PATH").unwrap_or_default()];
        entries.extend(DARWIN_FALLBACK_PATHS);
        env.set("PATH", merge_path_entries(&entries, ':'));
    }
    // 登录 shell 采集用 TERM=dumb / CI=1 避开交互分支；面板终端必须按交互终端启动。
    env.set("TERM", "xterm-256color");
    let color = env.non_empty("COLORTERM").unwrap_or("truecolor").to_owned();
    env.set("COLORTERM", color);
    if env.get("CI") == Some("1") && base.get("TERM") == Some("dumb") {
        env.remove("CI");
    }
    // GUI 启动可能继承不到 UTF-8 locale，中文路径会显示成转义乱码。
    for key in ["LANG", "LC_CTYPE"] {
        if is_missing_or_c_locale(env.get(key)) {
            env.set(key, fallback_locale.clone());
        }
    }
    if env.get("LC_ALL").is_some() && is_missing_or_c_locale(env.get("LC_ALL")) {
        env.set("LC_ALL", fallback_locale);
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(entries: &[(&str, &str)]) -> ProcessEnv {
        ProcessEnv::new(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn builds_an_interactive_utf8_environment() {
        let base = env(&[
            ("PATH", "/usr/bin:/custom/bin"),
            ("TERM", "dumb"),
            ("CI", "1"),
            ("LANG", "C"),
            ("LC_ALL", ""),
            ("COLORTERM", " 24bit "),
        ]);
        let mac = terminal_env(&base, true);
        assert_eq!(
            mac.get("PATH"),
            Some(
                "/usr/bin:/custom/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/bin:/usr/sbin:/sbin"
            )
        );
        assert_eq!(mac.get("TERM"), Some("xterm-256color"));
        assert_eq!(mac.get("COLORTERM"), Some("24bit"));
        assert_eq!(mac.get("CI"), None);
        assert_eq!(mac.get("LANG"), Some("en_US.UTF-8"));
        assert_eq!(mac.get("LC_CTYPE"), Some("en_US.UTF-8"));
        assert_eq!(mac.get("LC_ALL"), Some("en_US.UTF-8"));

        let linux = terminal_env(&env(&[("PATH", "/bin"), ("LC_CTYPE", "zh_CN.utf8")]), false);
        assert_eq!(linux.get("PATH"), Some("/bin"));
        assert_eq!(linux.get("LANG"), Some("zh_CN.utf8"));
        assert_eq!(linux.get("LC_ALL"), None);
        assert_eq!(linux.get("COLORTERM"), Some("truecolor"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_and_cwd_fall_back_when_invalid() {
        let base = env(&[
            ("SHELL", "/nonexistent/zsh"),
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/nonexistent-home"),
        ]);
        let shell = resolve_shell(&base).await.unwrap();
        assert!(["/bin/zsh", "/bin/bash", "/bin/sh"].contains(&shell.as_str()));
        let bare = env(&[("SHELL", "sh"), ("PATH", "/usr/bin:/bin")]);
        assert_eq!(resolve_shell(&bare).await.unwrap(), "sh");
        assert_eq!(
            resolve_cwd(Some("/nonexistent/dir"), &base).await.unwrap(),
            "/"
        );
        let dir = tempfile::tempdir().unwrap();
        let requested = dir.path().to_str().unwrap();
        assert_eq!(
            resolve_cwd(Some(requested), &base).await.unwrap(),
            requested
        );
    }
}
