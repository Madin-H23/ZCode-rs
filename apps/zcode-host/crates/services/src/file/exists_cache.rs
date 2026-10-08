//! `checkFilesExist` 的结果缓存（fileService.ts `FileExistenceCache`）：正负结果都缓存 60 s，
//! 最多 100 条，命中即移到队尾（LRU）；写入前先清理全部过期项。
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);
const MAX_ENTRIES: usize = 100;

/// 路径 → (是否存在, 过期时刻)，以及 LRU 顺序。
type Entries = (HashMap<String, (bool, Instant)>, VecDeque<String>);

#[derive(Default)]
pub struct ExistsCache {
    entries: Mutex<Entries>,
}

impl ExistsCache {
    fn get(&self, path: &str) -> Option<bool> {
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let (map, order) = &mut *guard;
        let (exists, expires) = *map.get(path)?;
        order.retain(|entry| entry != path);
        if expires <= Instant::now() {
            map.remove(path);
            return None;
        }
        order.push_back(path.to_owned());
        Some(exists)
    }

    fn set(&self, path: &str, exists: bool) {
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let (map, order) = &mut *guard;
        let now = Instant::now();
        map.retain(|_, (_, expires)| *expires > now);
        order.retain(|entry| map.contains_key(entry) && entry != path);
        map.insert(path.to_owned(), (exists, now + TTL));
        order.push_back(path.to_owned());
        while order.len() > MAX_ENTRIES {
            if let Some(oldest) = order.pop_front() {
                map.remove(&oldest);
            }
        }
    }

    /// 是否为普通文件（跟随符号链接）；结果进入缓存。
    pub async fn check(&self, path: &str) -> bool {
        if let Some(exists) = self.get(path) {
            return exists;
        }
        let exists = tokio::fs::metadata(path)
            .await
            .is_ok_and(|meta| meta.is_file());
        self.set(path, exists);
        exists
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn caches_both_results_and_bounds_size() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let path = file.to_str().unwrap();
        let cache = ExistsCache::default();
        assert!(!cache.check(path).await);
        std::fs::write(&file, "x").unwrap();
        // 与 Node 一样，缓存期内仍返回之前的否定结果。
        assert!(!cache.check(path).await);
        for index in 0..150 {
            cache.set(&format!("/p{index}"), true);
        }
        let guard = cache.entries.lock().unwrap();
        assert_eq!(guard.0.len(), MAX_ENTRIES);
        assert!(!guard.0.contains_key(path));
    }
}
