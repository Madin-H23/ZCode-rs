//! `.zcodeignore`（packages/services/src/file/workspaceFileIgnore.ts）。模板、标记行与分区变换
//! 必须与 TS 逐字节一致：两侧都会读写同一个文件，设置页按标记行精确切分。
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tokio::fs;
use zcode_host_kit::log;

pub const FILE_NAME: &str = ".zcodeignore";
const GITIGNORE: &str = ".gitignore";

const BUILTIN_IGNORE_LINES: &[&str] = &[
    ".git/",
    ".hg/",
    ".svn/",
    "node_modules/",
    "bower_components/",
    "jspm_packages/",
    "__pycache__/",
    "site-packages/",
    "venv/",
    "coverage/",
    "htmlcov/",
    "lcov-report/",
    "cmakefiles/",
    "cmake-build-*/",
    "bazel-*/",
    "pods/",
    "deriveddata/",
    "storybook-static/",
    "playwright-report/",
    "test-results/",
    "allure-results/",
    "allure-report/",
    "cdk.out/",
    "*.egg-info/",
    "*.dist-info/",
    "eggs/",
    "pip-wheel-metadata/",
    "wheels/",
];

const TEMPLATE_HEADER: &[&str] = &[
    "# ZCode 工作区文件搜索忽略规则（.zcodeignore）",
    "# 语法与 .gitignore 一致，只影响 ZCode 的 @ 文件候选 / Command Center / 文件树搜索，",
    "# 不影响文件树浏览、上传或 Agent 文件访问。",
    "# 修改 .gitignore 不会自动同步到本文件；可在设置页「从 .gitignore 同步」。",
    "",
];
const SYNC_MARKER: &str =
    "# ===== ↑ 以上同步自 .gitignore（「从 .gitignore 同步」只重写以上部分）=====";
const DEFAULTS_MARKER: &str =
    "# ----- ↑ 以上为 ZCode 默认排除规则（自定义规则请写在本行下方，不会被同步/恢复改动）-----";
const CUSTOM_SECTION_HINT: &str = "# 自定义规则写在下方（本行提示可删除）";

fn split_lines(content: &str) -> Vec<&str> {
    // JS `split(/\r?\n/)`
    content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn builtin_defaults_section(gitignore: Option<&str>) -> String {
    let Some(gitignore) = gitignore else {
        return BUILTIN_IGNORE_LINES.join("\n");
    };
    let mut declared = std::collections::HashSet::new();
    for raw in split_lines(gitignore) {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        declared.insert(line.to_owned());
        declared.insert(line.strip_suffix('/').unwrap_or(line).to_owned());
    }
    BUILTIN_IGNORE_LINES
        .iter()
        .filter(|line| {
            let bare = line.strip_suffix('/').unwrap_or(line);
            !declared.contains(**line) && !declared.contains(bare)
        })
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

fn gitignore_section(gitignore: Option<&str>) -> String {
    match gitignore {
        Some(content) if !content.trim().is_empty() => {
            if content.ends_with('\n') {
                content.to_owned()
            } else {
                format!("{content}\n")
            }
        }
        _ => format!("{}\n", TEMPLATE_HEADER.join("\n")),
    }
}

pub fn template(gitignore: Option<&str>) -> String {
    [
        gitignore_section(gitignore),
        SYNC_MARKER.to_owned(),
        builtin_defaults_section(gitignore),
        DEFAULTS_MARKER.to_owned(),
        CUSTOM_SECTION_HINT.to_owned(),
        String::new(),
    ]
    .join("\n")
}

struct Sections {
    gitignore: String,
    defaults: String,
    custom: String,
}

fn split_sections(content: &str) -> Option<Sections> {
    let lines = split_lines(content);
    let sync = lines.iter().position(|line| line.trim() == SYNC_MARKER)?;
    let defaults = lines
        .iter()
        .position(|line| line.trim() == DEFAULTS_MARKER)?;
    if defaults <= sync {
        return None;
    }
    Some(Sections {
        gitignore: lines[..sync].join("\n"),
        defaults: lines[sync + 1..defaults].join("\n").trim().to_owned(),
        custom: lines[defaults + 1..]
            .join("\n")
            .trim_start_matches('\n')
            .to_owned(),
    })
}

fn collapse_trailing_newlines(content: String) -> String {
    let trimmed = content.trim_end_matches('\n');
    if trimmed.len() == content.len() {
        content
    } else {
        format!("{trimmed}\n")
    }
}

fn sync_from_gitignore(current: &str, gitignore: Option<&str>) -> String {
    let Some(sections) = split_sections(current) else {
        return template(gitignore);
    };
    collapse_trailing_newlines(
        [
            gitignore_section(gitignore),
            SYNC_MARKER.to_owned(),
            sections.defaults,
            DEFAULTS_MARKER.to_owned(),
            sections.custom,
        ]
        .join("\n"),
    )
}

fn reset_defaults(current: &str, gitignore: Option<&str>) -> String {
    let Some(sections) = split_sections(current) else {
        return template(gitignore);
    };
    let defaults = builtin_defaults_section(Some(&sections.gitignore));
    collapse_trailing_newlines(
        [
            sections.gitignore,
            SYNC_MARKER.to_owned(),
            defaults,
            DEFAULTS_MARKER.to_owned(),
            sections.custom,
        ]
        .join("\n"),
    )
}

async fn read_optional(path: &Path) -> std::io::Result<Option<String>> {
    match fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 原子写（临时文件 0644、fsync 后 rename），设置页与扫描不会读到半截规则。
pub async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut suffix = [0u8; 6];
    let _ = getrandom::fill(&mut suffix);
    let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let temp: PathBuf = dir.join(format!(
        ".{}.{}.{millis}.{suffix}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = async {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o644);
        let mut file = options.open(&temp).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, content.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        fs::rename(&temp, path).await
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    result
}

pub struct Rules {
    matcher: Gitignore,
}

impl Rules {
    fn from_content(root: &Path, content: &str) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        for line in split_lines(content) {
            // 单行语法错误只跳过该行（与 npm ignore 宽松解析一致）。
            let _ = builder.add_line(None, line);
        }
        Self {
            matcher: builder.build().unwrap_or_else(|_| Gitignore::empty()),
        }
    }

    /// 相对路径（`/` 分隔）是否被忽略；目录按 dirOnly 规则匹配。
    pub fn ignores(&self, relative: &str, is_dir: bool) -> bool {
        self.matcher.matched(relative, is_dir).is_ignore()
    }
}

/// 扫描前加载规则；文件不存在时自动创建（与 TS 相同的副作用），失败则只在内存里使用。
pub async fn load(root: &Path) -> Rules {
    let ignore_path = root.join(FILE_NAME);
    match read_optional(&ignore_path).await {
        Ok(Some(existing)) => return Rules::from_content(root, &existing),
        Ok(None) => {}
        Err(error) => {
            log::warn(format!(
                "[workspace-file-ignore] 读取 {FILE_NAME} 失败，降级为内存规则: {error}"
            ));
            let gitignore = read_optional(&root.join(GITIGNORE)).await.ok().flatten();
            let content = gitignore.unwrap_or_else(|| template(None));
            return Rules::from_content(root, &content);
        }
    }
    let gitignore = read_optional(&root.join(GITIGNORE)).await.ok().flatten();
    let initial = template(gitignore.as_deref());
    match atomic_write(&ignore_path, &initial).await {
        Ok(()) => log::info(format!(
            "[workspace-file-ignore] 已自动创建 {FILE_NAME}（来源：{}）",
            if gitignore.is_some() {
                ".gitignore 拷贝"
            } else {
                "默认模板"
            }
        )),
        Err(error) => log::warn(format!(
            "[workspace-file-ignore] 自动创建 {FILE_NAME} 失败，降级为运行时规则: {error}"
        )),
    }
    Rules::from_content(root, &initial)
}

pub async fn read_for_settings(root: &Path) -> std::io::Result<(String, &'static str)> {
    if let Some(existing) = read_optional(&root.join(FILE_NAME)).await? {
        return Ok((existing, "file"));
    }
    let gitignore = read_optional(&root.join(GITIGNORE)).await.ok().flatten();
    Ok((template(gitignore.as_deref()), "template"))
}

pub async fn transform(root: &Path, kind: &str) -> String {
    let existing = read_optional(&root.join(FILE_NAME)).await.ok().flatten();
    let gitignore = read_optional(&root.join(GITIGNORE)).await.ok().flatten();
    let current = existing.unwrap_or_else(|| template(gitignore.as_deref()));
    if kind == "sync-gitignore" {
        sync_from_gitignore(&current, gitignore.as_deref())
    } else {
        reset_defaults(&current, gitignore.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_dedupes_defaults_against_gitignore() {
        let content = template(Some("node_modules\n/dist\n"));
        assert!(content.starts_with("node_modules\n/dist\n\n"));
        assert!(!content.contains("\nnode_modules/\n"));
        assert!(content.contains("\n.git/\n"));
        assert!(content.ends_with(&format!("{DEFAULTS_MARKER}\n{CUSTOM_SECTION_HINT}\n")));
    }

    #[test]
    fn transforms_keep_custom_section() {
        let current = format!("a\n{SYNC_MARKER}\n.git/\n{DEFAULTS_MARKER}\nmine/\n\n\n");
        let synced = sync_from_gitignore(&current, Some("b"));
        assert_eq!(
            synced,
            format!("b\n\n{SYNC_MARKER}\n.git/\n{DEFAULTS_MARKER}\nmine/\n")
        );
        let reset = reset_defaults(&current, None);
        assert!(reset.starts_with(&format!("a\n{SYNC_MARKER}\n.git/\n.hg/")));
        assert!(reset.ends_with(&format!("{DEFAULTS_MARKER}\nmine/\n")));
    }

    #[test]
    fn rules_match_directories_and_files() {
        let rules = Rules::from_content(Path::new("/w"), &template(None));
        assert!(rules.ignores("node_modules", true));
        assert!(!rules.ignores("node_modules", false));
        assert!(rules.ignores("pkg/cmake-build-debug", true));
        assert!(!rules.ignores("src/main.rs", false));
    }
}
