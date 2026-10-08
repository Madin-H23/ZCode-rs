//! `system` channel（packages/services/src/system/systemService.ts、integratedTerminalShells.ts）。
//! `probeIntranet` 依赖 HTTP 探测，随远端 / 内网能力一起移植，暂由契约 stub 应答。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use zcode_host_kit::{Reply, ServiceBuilder};

/// Node `process.platform` 的取值（UI 以它判断平台，例如 linux 下不开递归监听）。
pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn env_lookup<'a>(vars: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    vars.iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.as_str())
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Windows 下的 CMD 与 Git Bash；其他平台返回空列表（integratedTerminalShells.ts:12-125）。
pub fn integrated_terminal_shells(vars: &BTreeMap<String, String>, windows: bool) -> Vec<Value> {
    if !windows {
        return Vec::new();
    }
    let comspec = env_lookup(vars, "ComSpec")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("cmd.exe");
    let mut shells = vec![json!({
        "dialect": "cmd", "id": format!("cmd:{comspec}"), "label": "CMD", "path": comspec, "source": "system",
    })];
    let system_candidates = [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ];
    let git_bash = system_candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| is_file(path))
        .map(|path| (path, "system"))
        .or_else(|| {
            let path_var = env_lookup(vars, "PATH").unwrap_or("");
            let mut extensions: Vec<String> = env_lookup(vars, "PATHEXT")
                .unwrap_or(".COM;.EXE;.BAT;.CMD")
                .split(';')
                .map(|ext| ext.trim().to_lowercase())
                .filter(|ext| !ext.is_empty())
                .collect();
            if !extensions.iter().any(|ext| ext == ".exe") {
                extensions.insert(0, ".exe".into());
            }
            let git = path_var
                .split(';')
                .filter(|dir| !dir.trim().is_empty())
                .flat_map(|dir| {
                    extensions
                        .iter()
                        .map(move |ext| Path::new(dir.trim()).join(format!("git{ext}")))
                })
                .find(|path| is_file(path))?;
            let dir = git.parent()?.to_path_buf();
            [
                dir.join(r"..\bin\bash.exe"),
                dir.join(r"..\..\bin\bash.exe"),
            ]
            .into_iter()
            .find(|path| is_file(path))
            .map(|path| (path, "path"))
        });
    if let Some((path, source)) = git_bash {
        let path = path.display().to_string();
        shells.push(json!({
            "dialect": "git-bash", "id": format!("git-bash:{path}"), "label": "Git Bash", "path": path, "source": source,
        }));
    }
    shells
}

pub fn service(user_home: PathBuf, vars: BTreeMap<String, String>) -> ServiceBuilder {
    ServiceBuilder::new("system")
        .method("info", move |_, _| {
            let homedir = user_home.display().to_string();
            async move {
                Ok(Reply::Value(
                    json!({"homedir": homedir, "platform": node_platform()}),
                ))
            }
        })
        .method("listIntegratedTerminalShells", move |_, _| {
            let shells = integrated_terminal_shells(&vars, cfg!(windows));
            async move { Ok(Reply::Value(Value::Array(shells))) }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_are_windows_only_and_default_to_cmd() {
        assert!(integrated_terminal_shells(&BTreeMap::new(), false).is_empty());
        let shells = integrated_terminal_shells(&BTreeMap::new(), true);
        assert_eq!(shells[0]["id"], "cmd:cmd.exe");
        let vars = BTreeMap::from([("COMSPEC".to_owned(), r" C:\Windows\cmd.exe ".to_owned())]);
        assert_eq!(
            integrated_terminal_shells(&vars, true)[0]["path"],
            r"C:\Windows\cmd.exe"
        );
    }
}
