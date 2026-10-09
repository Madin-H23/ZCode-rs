//! 技能目录扫描策略（packages/shared/src/skill-scan-policy.ts）与有界遍历
//! （packages/services/src/skills/skillDiscoveryWalk.ts），skills 与 skill-sync 共用。
use std::collections::HashSet;

use super::fsx;

pub const SKILL_FILE_NAME: &str = "SKILL.md";
pub const MAX_SCAN_DEPTH: usize = 8;
const EXCLUDED: [&str; 12] = [
    "node_modules",
    "dist",
    "build",
    "out",
    "target",
    "vendor",
    "coverage",
    ".cache",
    ".next",
    ".turbo",
    ".venv",
    "__pycache__",
];

/// `shouldWalkSkillDirectoryEntry`：跳过内容目录与（除 `.system` 外的）点目录。
pub fn should_walk(name: &str) -> bool {
    if EXCLUDED.contains(&name) {
        return false;
    }
    !name.starts_with('.') || name == ".system"
}

/// 遍历中 readdir / stat 失败的位置与错误。
pub struct WalkError {
    pub path: String,
    pub message: String,
}

/// `walkSkillMarkdownPaths`：自根深度优先（栈），产出每个 `SKILL.md` 的路径。
pub async fn walk(root: &str) -> (Vec<String>, Vec<WalkError>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    let mut stack: Vec<(String, usize)> = vec![(root.to_owned(), 0)];
    let mut visited_links: HashSet<String> = HashSet::new();
    while let Some((dir, depth)) = stack.pop() {
        let entries = match fsx::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) => {
                errors.push(WalkError {
                    message: fsx::fs_message(&error, "scandir", &dir),
                    path: dir,
                });
                continue;
            }
        };
        let mut has_skill = false;
        let mut dirs = Vec::new();
        let mut links = Vec::new();
        for entry in entries {
            // 普通文件或指向文件的软链命名为 SKILL.md 都视为技能定义。
            if entry.name == SKILL_FILE_NAME && !entry.is_dir {
                has_skill = true;
                continue;
            }
            if !should_walk(&entry.name) {
                continue;
            }
            let path = fsx::join(&dir, &[&entry.name]);
            if entry.is_dir {
                dirs.push(path);
            } else if entry.is_symlink {
                links.push(path);
            }
        }
        if has_skill {
            found.push(fsx::join(&dir, &[SKILL_FILE_NAME]));
        }
        if depth >= MAX_SCAN_DEPTH {
            continue;
        }
        for child in dirs {
            stack.push((child, depth + 1));
        }
        // 软链目录：确认指向目录后按 realpath 去重，避免 junction / 环路重复或无限扫描。
        for link in links {
            let meta = match tokio::fs::metadata(&link).await {
                Ok(meta) => meta,
                Err(error) => {
                    errors.push(WalkError {
                        message: fsx::fs_message(&error, "stat", &link),
                        path: link,
                    });
                    continue;
                }
            };
            if !meta.is_dir() {
                continue;
            }
            let canonical = fsx::realpath_or_self(&link).await;
            if !visited_links.insert(canonical) {
                continue;
            }
            stack.push((link, depth + 1));
        }
    }
    (found, errors)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn walks_bounded_and_dedupes_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(root.join("node_modules/b")).unwrap();
        std::fs::write(root.join("node_modules/b/SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(root.join(".system/c")).unwrap();
        std::fs::write(root.join(".system/c/SKILL.md"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("link1")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("link2")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("a/loop")).unwrap();
        let (mut found, errors) = walk(&root.display().to_string()).await;
        found.sort();
        let names: Vec<String> = found
            .iter()
            .map(|path| {
                path.trim_start_matches(&root.display().to_string())
                    .to_owned()
            })
            .collect();
        assert!(errors.is_empty());
        assert!(names.contains(&"/a/SKILL.md".to_owned()));
        assert!(names.contains(&"/.system/c/SKILL.md".to_owned()));
        assert!(!names.iter().any(|name| name.contains("node_modules")));
        // link1/link2 指向同一目录，只走一次；loop 指回根，受深度与去重约束。
        let visited = |prefix: &str| names.iter().any(|name| name.starts_with(prefix));
        assert!(visited("/link1/") ^ visited("/link2/"));
    }
}
