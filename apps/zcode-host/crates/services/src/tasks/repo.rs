//! tasks-index 的读写端口：zcode-task、syncer 与 window-controller 只经它访问持久行，
//! 由 `tasks_index::TasksIndex` 实现（测试里换成内存实现）。
//!
//! 行一律是 `ZCodeTaskMeta` JSON；`apply_agent_patch` 的 patch 里，值为 null 的键表示清除该字段，
//! 缺席的键保持原值。
use futures_util::future::BoxFuture;
use serde_json::Value;

/// listTaskMetas 的过滤条件；provider 固定为 glm，已删除的行不返回。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoFilter {
    /// None 表示所有 workspace（`listPinnedTaskIds`）。
    pub workspace_key: Option<String>,
    pub pinned: Option<bool>,
    pub archived: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncOutcome {
    pub meta: Value,
    /// 本次写入首次初始化了 grouped 视图的顶层顺序（新任务第一次落库）。
    pub grouped_order_initialized: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnreadOutcome {
    /// 行不存在时为 None。
    pub meta: Option<Value>,
    pub changed: bool,
}

pub type RepoFuture<T> = BoxFuture<'static, anyhow::Result<T>>;

pub trait TaskRepo: Send + Sync {
    fn list(&self, filter: RepoFilter) -> RepoFuture<Vec<Value>>;
    fn deleted_ids(&self, workspace_key: String) -> RepoFuture<Vec<String>>;
    /// 只插入不存在的行，从不改已有行；返回插入数。
    fn seed_if_missing(&self, metas: Vec<Value>) -> RepoFuture<usize>;
    fn sync(&self, meta: Value, searchable_text: Option<String>) -> RepoFuture<Value>;
    fn sync_at_grouped_top(
        &self,
        meta: Value,
        searchable_text: Option<String>,
    ) -> RepoFuture<SyncOutcome>;
    /// 行不存在或已删除时返回 None，不写入。
    fn apply_agent_patch(
        &self,
        workspace_key: String,
        task_id: String,
        patch: Value,
    ) -> RepoFuture<Option<Value>>;
    fn set_unread(
        &self,
        workspace_key: String,
        task_id: String,
        unread: bool,
        expected_unread_at: Option<i64>,
    ) -> RepoFuture<UnreadOutcome>;
    fn set_pinned(
        &self,
        workspace_key: String,
        task_id: String,
        pinned: bool,
    ) -> RepoFuture<Option<Value>>;
    fn set_archived(
        &self,
        workspace_key: String,
        task_id: String,
        archived: bool,
    ) -> RepoFuture<Option<Value>>;
    fn mark_deleted(&self, workspace_key: String, task_id: String) -> RepoFuture<Option<Value>>;
    fn rename(
        &self,
        workspace_key: String,
        task_id: String,
        title: String,
    ) -> RepoFuture<Option<Value>>;
}

#[cfg(test)]
pub mod memory {
    //! 测试用的内存实现：语义只覆盖 service 与 syncer 依赖的部分。
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use serde_json::{Value, json};

    use super::*;

    /// (meta, pinned, archived, deleted, groupedOrder)
    type Row = (Value, bool, bool, bool, bool);

    #[derive(Default)]
    pub struct MemoryRepo {
        /// (workspaceKey, taskId) → 行
        pub rows: Mutex<BTreeMap<(String, String), Row>>,
    }

    fn key_of(meta: &Value) -> (String, String) {
        let key = meta["workspaceIdentity"]
            .as_str()
            .filter(|identity| !identity.trim().is_empty())
            .or_else(|| meta["workspacePath"].as_str())
            .unwrap_or_default()
            .to_owned();
        (key, meta["taskId"].as_str().unwrap_or_default().to_owned())
    }

    /// rowToMeta 不把 pinned / archived 列写回 meta；它们只体现在分区归属上。
    fn with_flags(meta: &Value, _pinned: bool, _archived: bool) -> Value {
        meta.clone()
    }

    impl MemoryRepo {
        fn update(
            &self,
            workspace_key: String,
            task_id: String,
            change: impl FnOnce(&mut Row),
        ) -> Option<Value> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows.get_mut(&(workspace_key, task_id))?;
            change(row);
            Some(with_flags(&row.0, row.1, row.2))
        }
    }

    impl TaskRepo for MemoryRepo {
        fn list(&self, filter: RepoFilter) -> RepoFuture<Vec<Value>> {
            let rows = self.rows.lock().unwrap();
            let list = rows
                .iter()
                .filter(|((key, _), (_, pinned, archived, deleted, _))| {
                    !deleted
                        && filter
                            .workspace_key
                            .as_ref()
                            .is_none_or(|wanted| wanted == key)
                        && filter.pinned.is_none_or(|wanted| wanted == *pinned)
                        && filter.archived.is_none_or(|wanted| wanted == *archived)
                })
                .map(|(_, (meta, pinned, archived, _, _))| with_flags(meta, *pinned, *archived))
                .collect();
            Box::pin(async move { Ok(list) })
        }
        fn deleted_ids(&self, workspace_key: String) -> RepoFuture<Vec<String>> {
            let rows = self.rows.lock().unwrap();
            let ids = rows
                .iter()
                .filter(|((key, _), row)| *key == workspace_key && row.3)
                .map(|((_, id), _)| id.clone())
                .collect();
            Box::pin(async move { Ok(ids) })
        }
        fn seed_if_missing(&self, metas: Vec<Value>) -> RepoFuture<usize> {
            let mut rows = self.rows.lock().unwrap();
            let mut inserted = 0;
            for meta in metas {
                rows.entry(key_of(&meta)).or_insert_with(|| {
                    inserted += 1;
                    (meta, false, false, false, false)
                });
            }
            Box::pin(async move { Ok(inserted) })
        }
        fn sync(&self, meta: Value, _: Option<String>) -> RepoFuture<Value> {
            let mut rows = self.rows.lock().unwrap();
            let row =
                rows.entry(key_of(&meta))
                    .or_insert((Value::Null, false, false, false, false));
            row.0 = meta.clone();
            Box::pin(async move { Ok(meta) })
        }
        fn sync_at_grouped_top(&self, meta: Value, _: Option<String>) -> RepoFuture<SyncOutcome> {
            let mut rows = self.rows.lock().unwrap();
            let row =
                rows.entry(key_of(&meta))
                    .or_insert((Value::Null, false, false, false, false));
            row.0 = meta.clone();
            let initialized = !row.4;
            row.4 = true;
            Box::pin(async move {
                Ok(SyncOutcome {
                    meta,
                    grouped_order_initialized: initialized,
                })
            })
        }
        fn apply_agent_patch(
            &self,
            workspace_key: String,
            task_id: String,
            patch: Value,
        ) -> RepoFuture<Option<Value>> {
            let mut rows = self.rows.lock().unwrap();
            let result = rows
                .get_mut(&(workspace_key, task_id))
                .filter(|row| !row.3)
                .map(|row| {
                    for (key, value) in patch.as_object().into_iter().flatten() {
                        match (value.is_null(), row.0.as_object_mut()) {
                            (true, Some(meta)) => {
                                meta.remove(key);
                            }
                            (false, Some(meta)) => {
                                meta.insert(key.clone(), value.clone());
                            }
                            _ => {}
                        }
                    }
                    with_flags(&row.0, row.1, row.2)
                });
            Box::pin(async move { Ok(result) })
        }
        fn set_unread(
            &self,
            workspace_key: String,
            task_id: String,
            unread: bool,
            _: Option<i64>,
        ) -> RepoFuture<UnreadOutcome> {
            let meta = self.update(workspace_key, task_id, |row| {
                row.0["unreadAt"] = if unread { json!(1) } else { Value::Null };
            });
            Box::pin(async move {
                Ok(UnreadOutcome {
                    changed: meta.is_some(),
                    meta,
                })
            })
        }
        fn set_pinned(&self, key: String, id: String, pinned: bool) -> RepoFuture<Option<Value>> {
            let meta = self.update(key, id, |row| row.1 = pinned);
            Box::pin(async move { Ok(meta) })
        }
        fn set_archived(
            &self,
            key: String,
            id: String,
            archived: bool,
        ) -> RepoFuture<Option<Value>> {
            let meta = self.update(key, id, |row| row.2 = archived);
            Box::pin(async move { Ok(meta) })
        }
        fn mark_deleted(&self, key: String, id: String) -> RepoFuture<Option<Value>> {
            let meta = self.update(key, id, |row| row.3 = true);
            Box::pin(async move { Ok(meta) })
        }
        fn rename(&self, key: String, id: String, title: String) -> RepoFuture<Option<Value>> {
            let meta = self.update(key, id, |row| {
                row.0["title"] = json!(title);
                row.0["titleOverridden"] = json!(true);
            });
            Box::pin(async move { Ok(meta) })
        }
    }
}
