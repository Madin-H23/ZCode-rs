//! workspace 文件索引与 `@` 搜索（fileService.ts:269-368、packages/shared/src/workspaceFileSearch.ts、
//! workspaceFileEntriesCodec.ts）。
//!
//! 索引只受根目录 `.zcodeignore` 约束（不读子目录 .gitignore）；被忽略的条目既不列出也不进入；
//! 符号链接目录只列出不跟随。打包格式与长度、切片单位（UTF-16 code unit）和 TS 一致，
//! Renderer 按 400 万字符分片拉取后拼接。
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::collate::locale_compare;
use super::ignore_rules::{self, Rules};

const CACHE_TTL: Duration = Duration::from_secs(60);
const CACHE_MAX: usize = 4;
pub const DISPLAY_CAP: usize = 1000;

const SKIPPED_FILE_NAMES: &[&str] = &["coverage.out", "lcov.info"];
const SKIPPED_FILE_EXTENSIONS: &[&str] = &[
    ".a",
    ".aar",
    ".beam",
    ".class",
    ".dll",
    ".dylib",
    ".ear",
    ".exe",
    ".gcda",
    ".gcno",
    ".gem",
    ".hi",
    ".idb",
    ".ilk",
    ".jar",
    ".lib",
    ".node",
    ".nupkg",
    ".o",
    ".obj",
    ".pdb",
    ".profdata",
    ".profraw",
    ".pyc",
    ".pyo",
    ".rlib",
    ".so",
    ".tsbuildinfo",
    ".war",
];

#[derive(Debug, Clone)]
struct Entry {
    relative: String,
    is_dir: bool,
}

pub struct WorkspaceIndex {
    entries: Vec<Entry>,
    /// 打包串的 UTF-16 编码；长度与切片都以 code unit 计。
    packed: Vec<u16>,
    candidates: OnceLock<(String, Vec<Candidate>)>,
}

struct Candidate {
    name: String,
    path: String,
    relative: String,
    is_dir: bool,
    lower_name: Vec<u16>,
    lower_relative: Vec<u16>,
    lower_path: Vec<u16>,
}

/// Node `path.extname` 对小写名的结果（不含点开头的整名，如 `.bashrc`）。
fn extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(0) | None => "",
        Some(index) => &name[index..],
    }
}

fn should_skip_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == ".env"
        || lower.starts_with(".env.")
        || SKIPPED_FILE_NAMES.contains(&lower.as_str())
        || SKIPPED_FILE_EXTENSIONS.contains(&extension(&lower))
}

fn escape(relative: &str) -> String {
    relative
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_skippable_read_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
    )
}

fn scan(root: &Path, rules: &Rules) -> std::io::Result<Vec<Entry>> {
    let mut stack = vec![root.to_path_buf()];
    let mut entries = Vec::new();
    while let Some(dir) = stack.pop() {
        let reader = match std::fs::read_dir(&dir) {
            Ok(reader) => reader,
            Err(error) if is_skippable_read_error(&error) => continue,
            Err(error) => return Err(error),
        };
        for item in reader {
            let Ok(item) = item else { continue };
            let path = item.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if relative == ignore_rules::FILE_NAME {
                continue;
            }
            let Ok(file_type) = item.file_type() else {
                continue;
            };
            let symlink = file_type.is_symlink();
            let is_dir = if symlink {
                std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir())
            } else {
                file_type.is_dir()
            };
            if rules.ignores(&relative, is_dir) {
                continue;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let include = if is_dir {
                !relative.split('/').any(|segment| segment.starts_with('.'))
            } else {
                !should_skip_file(&name)
            };
            if include {
                entries.push(Entry {
                    relative: relative.clone(),
                    is_dir,
                });
            }
            if is_dir && !symlink {
                stack.push(path);
            }
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| locale_compare(&a.relative, &b.relative))
    });
    Ok(entries)
}

fn pack(entries: &[Entry]) -> Vec<u16> {
    let text = entries
        .iter()
        .map(|entry| {
            format!(
                "{}\t{}",
                if entry.is_dir { "directory" } else { "file" },
                escape(&entry.relative)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    text.encode_utf16().collect()
}

fn utf16_lower_trim(text: &str) -> Vec<u16> {
    text.trim().to_lowercase().encode_utf16().collect()
}

fn find(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    (from..=haystack.len().checked_sub(needle.len())?)
        .find(|&index| &haystack[index..index + needle.len()] == needle)
}

/// `scoreNormalizedFuzzyMatch`：前缀 < 子串 < 子序列，长度与下标按 UTF-16 计。
fn fuzzy(text: &[u16], query: &[u16], query_chars: &[Vec<u16>]) -> Option<i64> {
    if text.is_empty() {
        return None;
    }
    let length_gap = text.len() as i64 - query.len() as i64;
    if text.starts_with(query) {
        return Some(length_gap);
    }
    if let Some(index) = find(text, query, 0) {
        return Some(100 + index as i64);
    }
    let mut score = 200i64;
    let mut start = 0usize;
    for unit in query_chars {
        let found = find(text, unit, start)?;
        score += (found - start) as i64;
        start = found + 1;
    }
    Some(score + length_gap)
}

impl WorkspaceIndex {
    pub fn packed_len(&self) -> usize {
        self.packed.len()
    }

    pub fn packed_range(&self, offset: usize, length: usize) -> String {
        let start = offset.min(self.packed.len());
        let end = offset.saturating_add(length).min(self.packed.len());
        String::from_utf16_lossy(&self.packed[start..end])
    }

    fn candidates(&self, root: &str) -> &[Candidate] {
        let (cached_root, candidates) = self.candidates.get_or_init(|| {
            let windows = root.contains('\\');
            let sep = if windows { "\\" } else { "/" };
            let candidates = self
                .entries
                .iter()
                .map(|entry| {
                    let relative = unescape(&escape(&entry.relative));
                    let name = relative.rsplit('/').next().unwrap_or(&relative).to_owned();
                    let native = if windows {
                        relative.replace('/', "\\")
                    } else {
                        relative.clone()
                    };
                    let path = format!("{root}{sep}{native}");
                    Candidate {
                        lower_name: utf16_lower_trim(&name),
                        lower_relative: utf16_lower_trim(&relative),
                        lower_path: utf16_lower_trim(&path),
                        name,
                        path,
                        relative,
                        is_dir: entry.is_dir,
                    }
                })
                .collect();
            (root.to_owned(), candidates)
        });
        debug_assert_eq!(cached_root, root);
        candidates
    }

    pub fn search(&self, root: &str, query: &str, limit: usize) -> Vec<Value> {
        let candidates = self.candidates(root);
        let to_json = |candidate: &Candidate| {
            json!({
                "name": candidate.name,
                "path": candidate.path,
                "relativePath": candidate.relative,
                "type": if candidate.is_dir { "directory" } else { "file" },
            })
        };
        let normalized = query.trim().to_lowercase();
        if normalized.is_empty() {
            let mut ordered: Vec<(usize, &Candidate)> = candidates.iter().enumerate().collect();
            ordered.sort_by_key(|(index, candidate)| (candidate.is_dir, *index));
            return ordered
                .into_iter()
                .take(limit)
                .map(|(_, candidate)| to_json(candidate))
                .collect();
        }
        let query_units: Vec<u16> = normalized.encode_utf16().collect();
        let query_chars: Vec<Vec<u16>> = normalized
            .chars()
            .map(|c| c.encode_utf16(&mut [0u16; 2]).to_vec())
            .collect();
        let mut scored: Vec<(i64, usize)> = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                let name = fuzzy(&candidate.lower_name, &query_units, &query_chars);
                let relative = fuzzy(&candidate.lower_relative, &query_units, &query_chars);
                let path = fuzzy(&candidate.lower_path, &query_units, &query_chars);
                let keyword = [relative.map(|s| s + 300), path.map(|s| s + 300)]
                    .into_iter()
                    .flatten()
                    .min();
                [name, relative.map(|s| s + 25), keyword]
                    .into_iter()
                    .flatten()
                    .min()
                    .map(|score| (score, index))
            })
            .collect();
        scored.sort();
        scored
            .into_iter()
            .take(limit)
            .map(|(_, index)| to_json(&candidates[index]))
            .collect()
    }
}

struct CacheEntry {
    key: String,
    signature: String,
    created: Instant,
    index: Arc<WorkspaceIndex>,
}

#[derive(Default)]
pub struct IndexCache {
    entries: Mutex<VecDeque<CacheEntry>>,
}

async fn signature(root: &Path) -> String {
    let ignore = root.join(ignore_rules::FILE_NAME);
    match tokio::fs::metadata(&ignore).await {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs_f64() * 1000.0)
                .unwrap_or_default();
            format!("{}\n{mtime}:{}", root.display(), meta.len())
        }
        Err(_) => format!("{}\nnone", root.display()),
    }
}

impl IndexCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<CacheEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub async fn get(
        &self,
        root: &Path,
        key: &str,
        refresh: bool,
    ) -> std::io::Result<Arc<WorkspaceIndex>> {
        // 每次都先加载规则：文件缺失时会自动创建，签名随之变化。
        let rules = ignore_rules::load(root).await;
        let signature = signature(root).await;
        {
            let mut entries = self.lock();
            entries.retain(|entry| entry.created.elapsed() < CACHE_TTL);
            if !refresh
                && let Some(position) = entries
                    .iter()
                    .position(|entry| entry.key == key && entry.signature == signature)
            {
                let entry = entries.remove(position).unwrap_or_else(|| unreachable!());
                let index = entry.index.clone();
                entries.push_back(entry);
                return Ok(index);
            }
        }
        let scan_root: PathBuf = root.to_path_buf();
        let entries_list = tokio::task::spawn_blocking(move || scan(&scan_root, &rules))
            .await
            .map_err(std::io::Error::other)??;
        let index = Arc::new(WorkspaceIndex {
            packed: pack(&entries_list),
            entries: entries_list,
            candidates: OnceLock::new(),
        });
        let mut entries = self.lock();
        entries.retain(|entry| entry.key != key);
        entries.push_back(CacheEntry {
            key: key.to_owned(),
            signature,
            created: Instant::now(),
            index: index.clone(),
        });
        while entries.len() > CACHE_MAX {
            entries.pop_front();
        }
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn index_respects_rules_and_packs_like_typescript() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::create_dir_all(base.join("node_modules/x")).unwrap();
        std::fs::create_dir_all(base.join(".github")).unwrap();
        std::fs::write(base.join("src/main.rs"), "").unwrap();
        std::fs::write(base.join("src/lib.o"), "").unwrap();
        std::fs::write(base.join(".env"), "").unwrap();
        std::fs::write(base.join(".github/ci.yml"), "").unwrap();
        std::fs::write(base.join("README.md"), "").unwrap();
        let cache = IndexCache::default();
        let index = cache.get(base, "k", false).await.unwrap();
        assert!(
            base.join(".zcodeignore").exists(),
            "rules file auto-created"
        );
        let packed = index.packed_range(0, index.packed_len());
        assert_eq!(
            packed,
            "directory\tsrc\nfile\t.github/ci.yml\nfile\tREADME.md\nfile\tsrc/main.rs"
        );
        assert_eq!(index.packed_range(10, 3), "src");

        let root_text = base.to_string_lossy().into_owned();
        let found = index.search(&root_text, "main", 10);
        assert_eq!(found[0]["relativePath"], "src/main.rs");
        assert_eq!(found[0]["path"], format!("{root_text}/src/main.rs"));
        let default_order = index.search(&root_text, "  ", 10);
        assert_eq!(default_order[0]["type"], "file");
        assert_eq!(default_order.last().unwrap()["type"], "directory");
    }

    #[test]
    fn fuzzy_scores_match_typescript_rules() {
        let units = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
        let chars = |text: &str| -> Vec<Vec<u16>> {
            text.chars()
                .map(|c| c.encode_utf16(&mut [0; 2]).to_vec())
                .collect()
        };
        assert_eq!(
            fuzzy(&units("main.rs"), &units("main"), &chars("main")),
            Some(3)
        );
        assert_eq!(
            fuzzy(&units("src/main.rs"), &units("main"), &chars("main")),
            Some(104)
        );
        // 子序列：m(0) a(+1) n(+2) → 200 + 3 + (6 - 3)
        assert_eq!(
            fuzzy(&units("mxaxxn"), &units("man"), &chars("man")),
            Some(206)
        );
        assert_eq!(fuzzy(&units("abc"), &units("z"), &chars("z")), None);
    }
}
