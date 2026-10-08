//! `TaskRepo` 的 tasks-index.sqlite 实现：把端口调用转给 [`TasksIndex`]。
//! 侧栏只看 provider = glm 且未删除的行（zcodeTaskServiceAdapter.ts 固定传 `provider: "glm"`）。
use serde_json::Value;

use super::meta::PROVIDER;
use super::repo::{RepoFilter, RepoFuture, SyncOutcome, TaskRepo, UnreadOutcome};
use crate::tasks_index::{ListFilter, TasksIndex};

impl TaskRepo for TasksIndex {
    fn list(&self, filter: RepoFilter) -> RepoFuture<Vec<Value>> {
        let index = self.clone();
        Box::pin(async move {
            index
                .list_task_metas(ListFilter {
                    workspace_key: filter.workspace_key,
                    include_deleted: false,
                    provider: Some(PROVIDER.to_owned()),
                    pinned: filter.pinned,
                    archived: filter.archived,
                })
                .await
        })
    }

    fn deleted_ids(&self, workspace_key: String) -> RepoFuture<Vec<String>> {
        let index = self.clone();
        Box::pin(async move {
            index
                .list_deleted_task_ids(&workspace_key, Some(PROVIDER))
                .await
        })
    }

    fn seed_if_missing(&self, metas: Vec<Value>) -> RepoFuture<usize> {
        let index = self.clone();
        Box::pin(async move { index.seed_task_metas_if_missing(metas).await })
    }

    fn sync(&self, meta: Value, searchable_text: Option<String>) -> RepoFuture<Value> {
        let index = self.clone();
        Box::pin(async move { index.sync_task_meta(meta, searchable_text).await })
    }

    fn sync_at_grouped_top(
        &self,
        meta: Value,
        searchable_text: Option<String>,
    ) -> RepoFuture<SyncOutcome> {
        let index = self.clone();
        Box::pin(async move {
            let outcome = index
                .sync_task_meta_at_grouped_top(meta, searchable_text)
                .await?;
            Ok(SyncOutcome {
                meta: outcome.meta,
                grouped_order_initialized: outcome.initialized_grouped_order,
            })
        })
    }

    fn apply_agent_patch(
        &self,
        workspace_key: String,
        task_id: String,
        patch: Value,
    ) -> RepoFuture<Option<Value>> {
        let index = self.clone();
        Box::pin(async move {
            index
                .apply_agent_patch(&workspace_key, &task_id, patch)
                .await
        })
    }

    fn set_unread(
        &self,
        workspace_key: String,
        task_id: String,
        unread: bool,
        expected_unread_at: Option<i64>,
    ) -> RepoFuture<UnreadOutcome> {
        let index = self.clone();
        Box::pin(async move {
            let outcome = index
                .set_unread(&workspace_key, &task_id, unread, expected_unread_at)
                .await?;
            Ok(UnreadOutcome {
                meta: outcome.meta,
                changed: outcome.changed,
            })
        })
    }

    fn set_pinned(
        &self,
        workspace_key: String,
        task_id: String,
        pinned: bool,
    ) -> RepoFuture<Option<Value>> {
        let index = self.clone();
        Box::pin(async move { index.set_pinned(&workspace_key, &task_id, pinned).await })
    }

    fn set_archived(
        &self,
        workspace_key: String,
        task_id: String,
        archived: bool,
    ) -> RepoFuture<Option<Value>> {
        let index = self.clone();
        Box::pin(async move { index.set_archived(&workspace_key, &task_id, archived).await })
    }

    fn mark_deleted(&self, workspace_key: String, task_id: String) -> RepoFuture<Option<Value>> {
        let index = self.clone();
        Box::pin(async move { index.mark_deleted(&workspace_key, &task_id).await })
    }

    fn rename(
        &self,
        workspace_key: String,
        task_id: String,
        title: String,
    ) -> RepoFuture<Option<Value>> {
        let index = self.clone();
        Box::pin(async move { index.rename(&workspace_key, &task_id, title).await })
    }
}
