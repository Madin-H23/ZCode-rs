//! Windows 安装目录守卫（packages/services/src/paths.ts `validateDataBaseDirTarget`）。
//!
//! 安装器 / 自动更新管理安装目录，把 `.zcode/v2` 放进去可能在升级时被覆盖；迁移前在 service 层拦截，
//! RPC 调用绕过前端也拦得住。判定是纯函数：平台、环境、cwd 都由调用方传入，任何平台都能单测。
use std::collections::BTreeMap;

use super::win32_path;

/// `DATA_BASE_DIR_FORBIDDEN_WINDOWS_INSTALL_DIR_ERROR_CODE`（packages/shared/src/settings-errors.ts）。
pub const FORBIDDEN_CODE: &str = "DATA_BASE_DIR_FORBIDDEN_WINDOWS_INSTALL_DIR";
/// Main 下发的实际安装目录（`ZCODE_WINDOWS_APP_INSTALL_DIR_ENV`）。
pub const INSTALL_DIR_ENV: &str = "ZCODE_WINDOWS_APP_INSTALL_DIR";

pub struct GuardInput<'a> {
    /// `process.platform === "win32"`；生产代码传 `cfg!(windows)`。
    pub windows: bool,
    /// 进程环境（候选目录与 `=X:` 盘符当前目录都从这里读）。
    pub env: &'a BTreeMap<String, String>,
    /// `options.appInstallDir`；Host 不传。
    pub app_install_dir: Option<&'a str>,
    /// `process.cwd()`：相对路径按它解析。
    pub cwd: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forbidden {
    /// 命中的候选目录原文（未规范化）。
    pub forbidden_dir: String,
}

/// JS `String.prototype.trim` 去掉的字符（WhiteSpace 与 LineTerminator）。
fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// JS `String.prototype.trim`。
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// `readEnvValue`：先按原键取，否则不分大小写取第一个非空值。
fn read_env_value<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    if let Some(value) = env.get(key).map(|value| js_trim(value))
        && !value.is_empty()
    {
        return Some(value);
    }
    let lower = key.to_lowercase();
    env.iter()
        .filter(|(candidate, _)| candidate.to_lowercase() == lower)
        .map(|(_, value)| js_trim(value))
        .find(|value| !value.is_empty())
}

fn trim_trailing_separators(value: &str) -> &str {
    value.trim_end_matches(['\\', '/'])
}

/// `normalizeWindowsComparablePath`。
fn comparable(path: &str, input: &GuardInput) -> Option<String> {
    let trimmed = js_trim(path);
    if trimmed.is_empty() {
        return None;
    }
    let normalized = win32_path::normalize(trimmed);
    let normalized = trim_trailing_separators(&normalized);
    if normalized.is_empty() {
        return None;
    }
    let resolved = win32_path::resolve(&[normalized], input.cwd, |key| {
        // Windows 的环境变量不分大小写：`=c:` 与 `=C:` 是同一个。
        input
            .env
            .get(key)
            .or_else(|| {
                input
                    .env
                    .iter()
                    .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
                    .map(|(_, value)| value)
            })
            .cloned()
    });
    Some(trim_trailing_separators(&resolved).to_lowercase())
}

/// `collectWindowsForbiddenAppInstallDirs`：按顺序，规范化后重复的只留第一个原文。
fn forbidden_dirs(input: &GuardInput) -> Vec<String> {
    let env = input.env;
    let under = |key: &str, rest: &[&str]| {
        read_env_value(env, key).map(|base| {
            let mut parts = vec![base];
            parts.extend_from_slice(rest);
            win32_path::join(&parts)
        })
    };
    let candidates = [
        input.app_install_dir.map(str::to_owned),
        read_env_value(env, INSTALL_DIR_ENV).map(str::to_owned),
        under("ProgramFiles", &["ZCode"]),
        under("ProgramFiles(x86)", &["ZCode"]),
        under("ProgramW6432", &["ZCode"]),
        under("LOCALAPPDATA", &["Programs", "ZCode"]),
    ];
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_empty() {
            continue;
        }
        let Some(normalized) = comparable(&candidate, input) else {
            continue;
        };
        if seen.insert(normalized) {
            result.push(candidate);
        }
    }
    result
}

/// `isWindowsPathEqualOrInside`。
fn equal_or_inside(path: &str, root: &str, input: &GuardInput) -> bool {
    let (Some(path), Some(root)) = (comparable(path, input), comparable(root, input)) else {
        return false;
    };
    path == root || path.starts_with(&format!("{root}\\"))
}

/// `validateDataBaseDirTarget(target)`：非 Windows 直接通过；命中任一安装目录候选即拒绝。
pub fn validate(target: &str, input: &GuardInput) -> Result<(), Forbidden> {
    if !input.windows {
        return Ok(());
    }
    for forbidden_dir in forbidden_dirs(input) {
        if equal_or_inside(target, &forbidden_dir, input) {
            return Err(Forbidden { forbidden_dir });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn check(
        target: &str,
        windows: bool,
        vars: &BTreeMap<String, String>,
    ) -> Result<(), Forbidden> {
        validate(
            target,
            &GuardInput {
                windows,
                env: vars,
                app_install_dir: None,
                cwd: "C:\\work",
            },
        )
    }

    #[test]
    fn windows_rejects_install_dir_and_children() {
        let vars = env(&[("ProgramFiles", "C:\\Program Files")]);
        let forbidden = Forbidden {
            forbidden_dir: "C:\\Program Files\\ZCode".to_owned(),
        };
        assert_eq!(
            check("C:\\Program Files\\ZCode", true, &vars),
            Err(forbidden.clone())
        );
        assert_eq!(
            check("c:/program files/zcode/data/", true, &vars),
            Err(forbidden)
        );
        assert_eq!(check("C:\\Program Files\\ZCodeData", true, &vars), Ok(()));
        assert_eq!(check("D:\\ZCode", true, &vars), Ok(()));
    }

    #[test]
    fn other_platforms_always_pass() {
        let vars = env(&[("ProgramFiles", "C:\\Program Files")]);
        assert_eq!(check("C:\\Program Files\\ZCode", false, &vars), Ok(()));
    }

    #[test]
    fn install_dir_env_is_reported_verbatim() {
        let vars = env(&[(INSTALL_DIR_ENV, "  D:\\Apps\\ZCode\\  ")]);
        assert_eq!(
            check("d:\\apps\\zcode\\x", true, &vars),
            Err(Forbidden {
                forbidden_dir: "D:\\Apps\\ZCode\\".to_owned()
            })
        );
    }

    #[test]
    fn host_platform_selection_uses_cfg() {
        // 生产调用点按编译目标选择平台：只有 Windows 构建才会拦截。
        let vars = env(&[("ProgramFiles", "C:\\Program Files")]);
        assert_eq!(
            check("C:\\Program Files\\ZCode", cfg!(windows), &vars).is_err(),
            cfg!(windows)
        );
    }

    #[test]
    fn js_trim_matches_string_prototype_trim() {
        assert_eq!(js_trim("\u{feff}\u{a0} x \n"), "x");
        assert_eq!(js_trim("\u{85}x"), "\u{85}x");
    }
}
